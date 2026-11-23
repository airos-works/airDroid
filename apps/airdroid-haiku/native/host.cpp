/*
 * airDroid's Be API side: the application, the windows Android draws in,
 * the main window (status, installed apps, actions), Deskbar launchers and
 * the clipboard. Rust drives it through the ad_* C functions below and
 * reads input and UI actions from an event queue (ad_host_wait /
 * ad_host_next_event); nothing here calls back into Rust.
 *
 * Keep AdEvent and the AD_* constants in step with src/host.rs.
 */

#include <Alert.h>
#include <Application.h>
#include <Bitmap.h>
#include <BitmapStream.h>
#include <Button.h>
#include <CheckBox.h>
#include <Clipboard.h>
#include <Cursor.h>
#include <algorithm>
#include <DataIO.h>
#include <Directory.h>
#include <Entry.h>
#include <File.h>
#include <FilePanel.h>
#include <FindDirectory.h>
#include <GroupLayout.h>
#include <GroupLayoutBuilder.h>
#include <LayoutBuilder.h>
#include <ListView.h>
#include <FormattingConventions.h>
#include <Language.h>
#include <Locale.h>
#include <LocaleRoster.h>
#include <Menu.h>
#include <MenuBar.h>
#include <MediaRecorder.h>
#include <MediaRoster.h>
#include <MenuField.h>
#include <MenuItem.h>
#include <Message.h>
#include <MessageRunner.h>
#include <Messenger.h>
#include <Mime.h>
#include <NodeInfo.h>
#include <Notification.h>
#include <String.h>
#include <TimeZone.h>
#include <OS.h>
#include <Path.h>
#include <PopUpMenu.h>
#include <Region.h>
#include <Roster.h>
#include <Screen.h>
#include <SoundPlayer.h>
#include <ScrollView.h>
#include <StatusBar.h>
#include <StringView.h>
#include <TextControl.h>
#include <TranslationUtils.h>
#include <View.h>
#include <Window.h>

#include <atomic>
#include <condition_variable>
#include <deque>
#include <map>
#include <mutex>
#include <string>
#include <vector>

#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>


extern "C" {

struct AdEvent {
	int32 kind;
	uint32 window;
	int32 buttons;
	int32 clicks;
	uint32 modifiers;
	int32 key;		// Haiku raw key code
	int32 repeat;
	float x;
	float y;
	float dx;
	float dy;
	uint32 text_len;
};

}

enum {
	AD_MOUSE_MOVED = 1,
	AD_MOUSE_DOWN = 2,
	AD_MOUSE_UP = 3,
	AD_MOUSE_EXITED = 4,
	AD_WHEEL = 5,
	AD_KEY_DOWN = 6,
	AD_KEY_UP = 7,
	AD_MODIFIERS = 8,
	AD_WINDOW_CLOSE = 9,
	AD_WINDOW_RESIZED = 10,
	AD_WINDOW_ACTIVATED = 11,
	AD_WINDOW_DEACTIVATED = 12,
	AD_REFS = 13,			// text: one path
	AD_ARGV = 14,			// text: arguments separated by '\0'
	AD_QUIT = 15,
	AD_ACTION = 16,			// key: action id below
	AD_APP_CLICKED = 17,	// text: package
	AD_CLIPBOARD = 18,		// text: clipboard text
	AD_TASK_BOUNDS = 19,	// x, y: left top; dx, dy: width height (screen)
};

// AD_ACTION ids (the main window's menu and buttons)
enum {
	AD_ACT_SHOW_FULL_UI = 1,
	AD_ACT_INSTALL_APK = 2,	// opens the file panel; the choice comes as AD_REFS
	AD_ACT_STOP = 3,
	AD_ACT_START = 4,
	AD_ACT_REMOVE_APP = 5,	// text: package
	AD_ACT_SETUP = 6,
	AD_ACT_TOGGLE_MULTIWINDOW = 7,
	AD_ACT_SETTINGS = 8,		// opens the settings window
	AD_ACT_SAVE_SETTINGS = 9,	// text: "key=value" lines
	AD_ACT_NEW_DATA = 10,		// set Android's data aside, start with new
};

static const uint32 kMsgAction = 'adAc';
static const uint32 kMsgAppInvoked = 'adAp';
static const uint32 kMsgRemoveApp = 'adRm';
static const uint32 kMsgPresent = 'adPr';
static const uint32 kMsgNewData = 'adNd';


// #pragma mark - event queue


namespace {

struct QueuedEvent {
	AdEvent event;
	std::string text;
};

std::mutex sQueueLock;
std::condition_variable sQueueCond;
std::deque<QueuedEvent> sQueue;
std::string sLastText;
bool sWake = false;


AdEvent
MakeEvent(int32 kind, uint32 window)
{
	AdEvent e;
	memset(&e, 0, sizeof(e));
	e.kind = kind;
	e.window = window;
	return e;
}


void
Push(const AdEvent& event, const std::string& text = std::string())
{
	std::lock_guard<std::mutex> lock(sQueueLock);
	// merge consecutive moves of one window: only the latest position counts
	if (event.kind == AD_MOUSE_MOVED && !sQueue.empty()) {
		QueuedEvent& last = sQueue.back();
		if (last.event.kind == AD_MOUSE_MOVED
			&& last.event.window == event.window
			&& last.event.buttons == event.buttons) {
			last.event = event;
			sQueueCond.notify_one();
			return;
		}
	}
	QueuedEvent q;
	q.event = event;
	q.event.text_len = text.size();
	q.text = text;
	sQueue.push_back(q);
	sQueueCond.notify_one();
}

}	// namespace


// #pragma mark - Android windows


class AndroidView : public BView {
public:
	AndroidView(BRect frame, uint32 id)
		:
		BView(frame, "android", B_FOLLOW_ALL_SIDES,
			B_WILL_DRAW | B_FRAME_EVENTS | B_NAVIGABLE),
		fId(id),
		fBitmap(NULL),
		fFront(0),
		fCursorHidden(false),
		fBorderless(false),
		fResizeEdges(0),
		fResizing(false)
	{
		SetViewColor(B_TRANSPARENT_COLOR);
		SetDrawingMode(B_OP_COPY);
	}

	~AndroidView()
	{
		delete fBitmap;
		for (int i = 0; i < 2; i++)
			_DeleteFrame(i);
	}

	/*!	Double-buffered frames: the back bitmap of \a width x \a height
		(packed B_RGB32 rows) for the engine to compose into without the
		window locked, then EndFrame() makes it the front one. BeginFrame()
		syncs with app_server first, so it is done with a bitmap before we
		write into it again. Called with the window locked. */
	uint32* BeginFrame(int32 width, int32 height, area_id* _area)
	{
		for (int i = 0; i < 2; i++) {
			if (fFrames[i] == NULL
				|| fFrames[i]->Bounds().IntegerWidth() + 1 != width
				|| fFrames[i]->Bounds().IntegerHeight() + 1 != height) {
				_DeleteFrame(i);
				BRect bounds(0, 0, width - 1, height - 1);
				// in an area of our own that QEMU may clone, so it can read
				// GPU frames straight into it; app_server clones it too
				size_t size = ((size_t)width * 4 * height + B_PAGE_SIZE - 1)
					& ~(size_t)(B_PAGE_SIZE - 1);
				void* address = NULL;
				fAreas[i] = create_area("airDroid frame", &address,
					B_ANY_ADDRESS, size, B_NO_LOCK,
					B_READ_AREA | B_WRITE_AREA | B_CLONEABLE_AREA);
				if (fAreas[i] >= B_OK) {
					fFrames[i] = new BBitmap(fAreas[i], 0, bounds, 0, B_RGB32,
						width * 4);
					if (fFrames[i]->InitCheck() != B_OK) {
						_DeleteFrame(i);
					}
				}
				if (fFrames[i] == NULL) {
					fFrames[i] = new BBitmap(bounds, 0, B_RGB32);
				}
				if (fFrames[i]->InitCheck() != B_OK
					|| fFrames[i]->BytesPerRow() != width * 4) {
					_DeleteFrame(i);
					return NULL;
				}
			}
		}
		delete fBitmap;
		fBitmap = NULL;
		// app_server must be done drawing the back bitmap (it was the front
		// one last time): what we sent it so far is handled before this
		// returns. (Not from Draw(): inside an update app_server answers it
		// only after the update, which waits for Draw().)
		Sync();
		*_area = fAreas[1 - fFront];
		return (uint32*)fFrames[1 - fFront]->Bits();
	}

	void _DeleteFrame(int i)
	{
		delete fFrames[i];
		fFrames[i] = NULL;
		if (fAreas[i] >= B_OK)
			delete_area(fAreas[i]);
		fAreas[i] = -1;
	}

	void EndFrame()
	{
		fFront = 1 - fFront;
		Invalidate();
	}

	// Called with the window locked.
	void Present(const uint32* pixels, int32 width, int32 height)
	{
		if (fBitmap == NULL || fBitmap->Bounds().IntegerWidth() + 1 != width
			|| fBitmap->Bounds().IntegerHeight() + 1 != height) {
			delete fBitmap;
			// a server-linked bitmap: app_server draws straight from the
			// area we write (B_BITMAP_NO_SERVER_LINK ones can't be drawn)
			fBitmap = new BBitmap(BRect(0, 0, width - 1, height - 1), 0,
				B_RGB32);
		}
		uint8* bits = (uint8*)fBitmap->Bits();
		int32 bpr = fBitmap->BytesPerRow();
		if (bpr == width * 4) {
			memcpy(bits, pixels, (size_t)width * height * 4);
		} else {
			for (int32 y = 0; y < height; y++)
				memcpy(bits + y * bpr, pixels + (size_t)y * width, width * 4);
		}
		Invalidate();
	}

	virtual void Draw(BRect updateRect)
	{
		BRect painted;
		BBitmap* bitmap = fFrames[fFront] != NULL ? fFrames[fFront] : fBitmap;
		if (bitmap != NULL) {
			painted = bitmap->Bounds() & updateRect;
			if (painted.IsValid())
				DrawBitmapAsync(bitmap, painted, painted);
		}
		SetHighColor(0, 0, 0);
		BRegion rest(updateRect);
		if (painted.IsValid())
			rest.Exclude(painted);
		FillRegion(&rest);
	}

	void SetBorderless(bool borderless) { fBorderless = borderless; }

	// Multi-window windows have no border: their edges resize them, and
	// Android then gives the app's task the new bounds.
	enum { kLeft = 1, kTop = 2, kRight = 4, kBottom = 8 };
	static const int kEdge = 6;

	uint32 EdgesAt(BPoint where) const
	{
		if (!fBorderless)
			return 0;
		BRect b = Bounds();
		uint32 edges = 0;
		if (where.x < b.left + kEdge)
			edges |= kLeft;
		else if (where.x > b.right - kEdge)
			edges |= kRight;
		if (where.y < b.top + kEdge)
			edges |= kTop;
		else if (where.y > b.bottom - kEdge)
			edges |= kBottom;
		return edges;
	}

	void SetEdgeCursor(uint32 edges)
	{
		if (edges == fResizeEdges)
			return;
		fResizeEdges = edges;
		BCursorID id = B_CURSOR_ID_SYSTEM_DEFAULT;
		switch (edges) {
			case kLeft: case kRight: id = B_CURSOR_ID_RESIZE_EAST_WEST; break;
			case kTop: case kBottom: id = B_CURSOR_ID_RESIZE_NORTH_SOUTH; break;
			case kLeft | kTop: case kRight | kBottom:
				id = B_CURSOR_ID_RESIZE_NORTH_WEST_SOUTH_EAST; break;
			case kRight | kTop: case kLeft | kBottom:
				id = B_CURSOR_ID_RESIZE_NORTH_EAST_SOUTH_WEST; break;
		}
		if (id == B_CURSOR_ID_SYSTEM_DEFAULT && fCursorHidden) {
			BCursor cursor(B_CURSOR_ID_NO_CURSOR);
			SetViewCursor(&cursor);
		} else {
			BCursor cursor(id);
			SetViewCursor(&cursor);
		}
	}

	virtual void MouseDown(BPoint where)
	{
		MakeFocus(true);
		SetMouseEventMask(B_POINTER_EVENTS, B_NO_POINTER_HISTORY);
		uint32 edges = EdgesAt(where);
		if (edges != 0) {
			SetEdgeCursor(edges);
			fResizing = true;
			fResizeStart = ConvertToScreen(where);
			fStartFrame = Window()->Frame();
			return;
		}
		AdEvent event = MakeEvent(AD_MOUSE_DOWN, fId);
		event.x = where.x;
		event.y = where.y;
		BMessage* message = Window()->CurrentMessage();
		if (message != NULL) {
			message->FindInt32("buttons", &event.buttons);
			message->FindInt32("clicks", &event.clicks);
		}
		event.modifiers = modifiers();
		Push(event);
	}

	virtual void MouseUp(BPoint where)
	{
		if (fResizing) {
			fResizing = false;
			BRect f = Window()->Frame();
			AdEvent event = MakeEvent(AD_TASK_BOUNDS, fId);
			event.x = f.left;
			event.y = f.top;
			event.dx = f.Width() + 1;
			event.dy = f.Height() + 1;
			Push(event);
			return;
		}
		AdEvent event = MakeEvent(AD_MOUSE_UP, fId);
		event.x = where.x;
		event.y = where.y;
		BMessage* message = Window()->CurrentMessage();
		if (message != NULL)
			message->FindInt32("buttons", &event.buttons);
		Push(event);
	}

	virtual void MouseMoved(BPoint where, uint32 transit,
		const BMessage* dragMessage)
	{
		if (fResizing) {
			BPoint p = ConvertToScreen(where);
			float dx = p.x - fResizeStart.x, dy = p.y - fResizeStart.y;
			BRect f = fStartFrame;
			const float kMinW = 160, kMinH = 120;
			if (fResizeEdges & kLeft)
				f.left = std::min(f.left + dx, f.right - kMinW);
			if (fResizeEdges & kRight)
				f.right = std::max(f.right + dx, f.left + kMinW);
			if (fResizeEdges & kTop)
				f.top = std::min(f.top + dy, f.bottom - kMinH);
			if (fResizeEdges & kBottom)
				f.bottom = std::max(f.bottom + dy, f.top + kMinH);
			Window()->MoveTo(f.LeftTop());
			Window()->ResizeTo(f.Width(), f.Height());
			return;
		}
		if (transit == B_EXITED_VIEW) {
			SetEdgeCursor(0);
			Push(MakeEvent(AD_MOUSE_EXITED, fId));
			return;
		}
		uint32 edges = EdgesAt(where);
		SetEdgeCursor(edges);
		if (edges != 0)
			return;
		AdEvent event = MakeEvent(AD_MOUSE_MOVED, fId);
		event.x = where.x;
		event.y = where.y;
		BMessage* message = Window()->CurrentMessage();
		if (message != NULL)
			message->FindInt32("buttons", &event.buttons);
		Push(event);
	}


	virtual void MessageReceived(BMessage* message)
	{
		switch (message->what) {
			case B_MOUSE_WHEEL_CHANGED:
			{
				AdEvent event = MakeEvent(AD_WHEEL, fId);
				float dx = 0, dy = 0;
				message->FindFloat("be:wheel_delta_x", &dx);
				message->FindFloat("be:wheel_delta_y", &dy);
				event.dx = dx;
				event.dy = dy;
				Push(event);
				break;
			}
			case B_MODIFIERS_CHANGED:
			{
				AdEvent event = MakeEvent(AD_MODIFIERS, fId);
				int32 mods = 0, old = 0;
				message->FindInt32("modifiers", &mods);
				message->FindInt32("be:old_modifiers", &old);
				event.modifiers = mods;
				event.key = old;
				Push(event);
				break;
			}
			default:
				BView::MessageReceived(message);
		}
	}

	virtual void FrameResized(float width, float height)
	{
		AdEvent event = MakeEvent(AD_WINDOW_RESIZED, fId);
		event.x = width + 1;
		event.y = height + 1;
		Push(event);
	}

	void SetCursorHidden(bool hidden)
	{
		if (hidden == fCursorHidden)
			return;
		fCursorHidden = hidden;
		if (fResizeEdges != 0)
			return;
		if (hidden) {
			BCursor cursor(B_CURSOR_ID_NO_CURSOR);
			SetViewCursor(&cursor);
		} else
			SetViewCursor(B_CURSOR_SYSTEM_DEFAULT);
	}

	// Every key message of the window comes here (AndroidWindow::
	// DispatchMessage), so Android sees Command/Control combinations too.
	void KeyMessage(BMessage* message)
	{
		int32 kind = (message->what == B_KEY_DOWN
			|| message->what == B_UNMAPPED_KEY_DOWN) ? AD_KEY_DOWN : AD_KEY_UP;
		AdEvent event = MakeEvent(kind, fId);
		message->FindInt32("key", &event.key);
		int32 mods = 0;
		message->FindInt32("modifiers", &mods);
		event.modifiers = mods;
		message->FindInt32("be:key_repeat", &event.repeat);
		Push(event);
	}

private:
	uint32		fId;
	BBitmap*	fBitmap;
	BBitmap*	fFrames[2] = { NULL, NULL };
	area_id		fAreas[2] = { -1, -1 };
	int			fFront;
	bool		fCursorHidden;
	bool		fBorderless;
	uint32		fResizeEdges;
	bool		fResizing;
	BPoint		fResizeStart;
	BRect		fStartFrame;
};


class AndroidWindow : public BWindow {
public:
	AndroidWindow(BRect frame, const char* title, uint32 id, bool borderless)
		:
		BWindow(frame, title,
			borderless ? B_NO_BORDER_WINDOW_LOOK : B_TITLED_WINDOW_LOOK,
			B_NORMAL_WINDOW_FEEL, B_ASYNCHRONOUS_CONTROLS
				| (borderless ? B_NOT_RESIZABLE | B_NOT_ZOOMABLE : 0)),
		fId(id)
	{
		fView = new AndroidView(Bounds(), id);
		fView->SetBorderless(borderless);
		AddChild(fView);
		fView->MakeFocus(true);
		// Android keeps every key: none of BWindow's own Command shortcuts
		RemoveShortcut('X', B_COMMAND_KEY);
		RemoveShortcut('C', B_COMMAND_KEY);
		RemoveShortcut('V', B_COMMAND_KEY);
		RemoveShortcut('A', B_COMMAND_KEY);
		RemoveShortcut('W', B_COMMAND_KEY);
		RemoveShortcut('Q', B_COMMAND_KEY);
	}

	AndroidView* View() const { return fView; }

	virtual bool QuitRequested()
	{
		// Android decides: it may close the task (the guest then destroys
		// the window) or not
		Push(MakeEvent(AD_WINDOW_CLOSE, fId));
		return false;
	}

	virtual void WindowActivated(bool active)
	{
		Push(MakeEvent(active ? AD_WINDOW_ACTIVATED : AD_WINDOW_DEACTIVATED,
			fId));
	}

	virtual void DispatchMessage(BMessage* message, BHandler* handler)
	{
		// BWindow eats Command+key for shortcuts it doesn't have; Android
		// wants them (Ctrl/Alt combinations)
		if (message->what == B_KEY_DOWN || message->what == B_KEY_UP
			|| message->what == B_UNMAPPED_KEY_DOWN
			|| message->what == B_UNMAPPED_KEY_UP) {
			fView->KeyMessage(message);
			return;
		}
		BWindow::DispatchMessage(message, handler);
	}

	virtual void MessageReceived(BMessage* message)
	{
		BWindow::MessageReceived(message);
	}

private:
	uint32			fId;
	AndroidView*	fView;
};


// #pragma mark - main window


struct AppEntry {
	std::string package;
	std::string name;
	BBitmap* icon;
};


class AppItem : public BListItem {
public:
	AppItem(const AppEntry& entry)
		:
		fEntry(entry)
	{
	}

	~AppItem()
	{
		delete fEntry.icon;
	}

	const std::string& Package() const { return fEntry.package; }

	virtual void Update(BView* owner, const BFont* font)
	{
		BListItem::Update(owner, font);
		SetHeight(std::max(Height(), 40.0f));
	}

	virtual void DrawItem(BView* owner, BRect frame, bool complete)
	{
		rgb_color background = IsSelected()
			? ui_color(B_LIST_SELECTED_BACKGROUND_COLOR)
			: ui_color(B_LIST_BACKGROUND_COLOR);
		owner->SetHighColor(background);
		owner->FillRect(frame);
		if (fEntry.icon != NULL) {
			owner->SetDrawingMode(B_OP_ALPHA);
			owner->DrawBitmap(fEntry.icon, fEntry.icon->Bounds(),
				BRect(frame.left + 4, frame.top + 4, frame.left + 35,
					frame.top + 35), B_FILTER_BITMAP_BILINEAR);
			owner->SetDrawingMode(B_OP_COPY);
		}
		owner->SetHighColor(IsSelected()
			? ui_color(B_LIST_SELECTED_ITEM_TEXT_COLOR)
			: ui_color(B_LIST_ITEM_TEXT_COLOR));
		font_height fh;
		owner->GetFontHeight(&fh);
		float baseline = frame.top + (frame.Height() + fh.ascent - fh.descent) / 2;
		owner->DrawString(fEntry.name.c_str(), BPoint(frame.left + 44, baseline));
		owner->SetHighColor(tint_color(owner->HighColor(), B_LIGHTEN_1_TINT));
	}

private:
	AppEntry fEntry;
};


class MainWindow : public BWindow {
public:
	MainWindow()
		:
		BWindow(BRect(100, 100, 520, 600), "airDroid", B_TITLED_WINDOW,
			B_ASYNCHRONOUS_CONTROLS | B_AUTO_UPDATE_SIZE_LIMITS)
	{
		BMenuBar* bar = new BMenuBar("menu");
		BMenu* file = new BMenu("File");
		file->AddItem(new BMenuItem("Install App" B_UTF8_ELLIPSIS,
			_Action(AD_ACT_INSTALL_APK), 'O'));
		file->AddSeparatorItem();
		file->AddItem(new BMenuItem("Quit", new BMessage(B_QUIT_REQUESTED), 'Q'));
		bar->AddItem(file);
		BMenu* android = new BMenu("Android");
		android->AddItem(new BMenuItem("Show Android",
			_Action(AD_ACT_SHOW_FULL_UI), 'A', B_SHIFT_KEY));
		fMultiItem = new BMenuItem("Apps in their own windows",
			_Action(AD_ACT_TOGGLE_MULTIWINDOW));
		android->AddItem(fMultiItem);
		android->AddSeparatorItem();
		android->AddItem(new BMenuItem("Start", _Action(AD_ACT_START)));
		android->AddItem(new BMenuItem("Stop", _Action(AD_ACT_STOP)));
		android->AddItem(new BMenuItem("Download Android again" B_UTF8_ELLIPSIS,
			_Action(AD_ACT_SETUP)));
		android->AddItem(new BMenuItem("Start With New Data" B_UTF8_ELLIPSIS,
			new BMessage(kMsgNewData)));
		android->AddSeparatorItem();
		android->AddItem(new BMenuItem("Settings" B_UTF8_ELLIPSIS,
			_Action(AD_ACT_SETTINGS), ','));
		bar->AddItem(android);

		fStatus = new BStringView("status", "Starting" B_UTF8_ELLIPSIS);
		// long messages (errors) must not widen the window
		fStatus->SetTruncation(B_TRUNCATE_END);
		fStatus->SetExplicitMinSize(BSize(200, B_SIZE_UNSET));
		fProgress = new BStatusBar("progress");
		fProgress->SetMaxValue(1.0);
		fProgress->Hide();
		fApps = new BListView("apps");
		fApps->SetInvocationMessage(new BMessage(kMsgAppInvoked));
		BScrollView* scroll = new BScrollView("scroll", fApps, 0, false, true);
		fShowButton = new BButton("show", "Show Android",
			_Action(AD_ACT_SHOW_FULL_UI));
		BButton* install = new BButton("install", "Install App" B_UTF8_ELLIPSIS,
			_Action(AD_ACT_INSTALL_APK));

		BLayoutBuilder::Group<>(this, B_VERTICAL, 0)
			.Add(bar)
			.AddGroup(B_VERTICAL, B_USE_DEFAULT_SPACING)
				.SetInsets(B_USE_WINDOW_SPACING)
				.Add(fStatus)
				.Add(fProgress)
				.Add(scroll, 1.0)
				.AddGroup(B_HORIZONTAL)
					.Add(install)
					.AddGlue()
					.Add(fShowButton)
				.End()
			.End();
		SetSizeLimits(320, 4000, 300, 4000);
	}

	virtual bool QuitRequested()
	{
		Push(MakeEvent(AD_QUIT, 0));
		return false;
	}

	virtual void MessageReceived(BMessage* message)
	{
		switch (message->what) {
			case kMsgAction:
			{
				AdEvent event = MakeEvent(AD_ACTION, 0);
				message->FindInt32("action", &event.key);
				Push(event);
				break;
			}
			case kMsgNewData:
			{
				BAlert* alert = new BAlert("airDroid",
					"Start Android with new data?\n\n"
					"Android starts as it was at first: without the apps "
					"installed since and without their data. Nothing is "
					"deleted: what is there now is kept in a file next to "
					"the new data (in config/non-packaged/data/airDroid), "
					"and renaming that file back to userdata.qcow2 brings "
					"it back.",
					"Cancel", "Start With New Data", NULL,
					B_WIDTH_AS_USUAL, B_WARNING_ALERT);
				alert->SetShortcut(0, B_ESCAPE);
				if (alert->Go() == 1) {
					AdEvent event = MakeEvent(AD_ACTION, 0);
					event.key = AD_ACT_NEW_DATA;
					Push(event);
				}
				break;
			}
			case kMsgAppInvoked:
			{
				int32 index = fApps->CurrentSelection();
				AppItem* item = dynamic_cast<AppItem*>(fApps->ItemAt(index));
				if (item != NULL)
					Push(MakeEvent(AD_APP_CLICKED, 0), item->Package());
				break;
			}
			default:
				BWindow::MessageReceived(message);
		}
	}

	void SetStatus(const char* text, float progress)
	{
		fStatus->SetText(text);
		if (progress < 0) {
			if (!fProgress->IsHidden())
				fProgress->Hide();
		} else {
			if (fProgress->IsHidden())
				fProgress->Show();
			fProgress->SetTo(progress);
		}
	}

	void SetApps(std::vector<AppEntry>& apps)
	{
		while (BListItem* item = fApps->RemoveItem((int32)0))
			delete item;
		for (size_t i = 0; i < apps.size(); i++)
			fApps->AddItem(new AppItem(apps[i]));
		apps.clear();
	}

	void SetMultiWindow(bool on)
	{
		fMultiItem->SetMarked(on);
	}

private:
	static BMessage* _Action(int32 action)
	{
		BMessage* m = new BMessage(kMsgAction);
		m->AddInt32("action", action);
		return m;
	}

	BStringView*	fStatus;
	BStatusBar*		fProgress;
	BListView*		fApps;
	BButton*		fShowButton;
	BMenuItem*		fMultiItem;
};


// #pragma mark - settings


static const uint32 kMsgSettingsSave = 'adSv';
static const uint32 kMsgShareToggled = 'adSt';
static const uint32 kMsgShareChoose = 'adSc';
static const uint32 kMsgShareChosen = 'adSC';


/*! Android's VM and display: memory, processors, graphics, density, mouse
	as touch and the shared folder. Saving sends them to the engine as
	"key=value" lines (AD_ACT_SAVE_SETTINGS); they apply when Android
	starts next. */
class SettingsWindow : public BWindow {
public:
	SettingsWindow(uint32 ramMB, uint32 cpus, uint32 gpu, uint32 dpi,
			bool touch, const char* share)
		:
		BWindow(BRect(140, 140, 600, 460), "airDroid settings",
			B_TITLED_WINDOW, B_ASYNCHRONOUS_CONTROLS | B_NOT_ZOOMABLE
				| B_NOT_RESIZABLE | B_AUTO_UPDATE_SIZE_LIMITS
				| B_CLOSE_ON_ESCAPE),
		fPanel(NULL)
	{
		system_info info;
		get_system_info(&info);
		uint64 totalMB = info.max_pages * (uint64)B_PAGE_SIZE / (1024 * 1024);
		uint32 maxCpus = info.cpu_count;

		fRam = new BPopUpMenu("ram");
		const uint32 sizes[] = { 2048, 3072, 4096, 6144, 8192, 12288, 16384 };
		for (uint32 size : sizes) {
			// at most three quarters of the machine (and the current value)
			if (size > totalMB * 3 / 4 && size != ramMB)
				continue;
			BString label;
			label << size / 1024 << " GB";
			_AddItem(fRam, label, size, size == ramMB);
		}
		fCpus = new BPopUpMenu("cpus");
		for (uint32 n = 1; n <= std::max(maxCpus, cpus); n++) {
			BString label;
			label << n;
			_AddItem(fCpus, label, n, n == cpus);
		}
		fGpu = new BPopUpMenu("gpu");
		_AddItem(fGpu, "Automatic", 0, gpu == 0);
		_AddItem(fGpu, "The GPU (virgl)", 1, gpu == 1);
		_AddItem(fGpu, "The GPU, Vulkan too (experimental)", 3, gpu == 3);
		_AddItem(fGpu, "Software (SwiftShader)", 2, gpu == 2);
		fDpi = new BPopUpMenu("dpi");
		const uint32 densities[] = { 0, 120, 160, 213, 240, 280, 320 };
		for (uint32 d : densities) {
			BString label;
			if (d == 0)
				label = "Android's default";
			else
				label << d << " dpi";
			_AddItem(fDpi, label, d, d == dpi);
		}
		fTouch = new BCheckBox("touch", "Mouse clicks are touches", NULL);
		fTouch->SetValue(touch ? B_CONTROL_ON : B_CONTROL_OFF);
		fShare = new BCheckBox("share",
			"Share a folder with Android (as /sdcard/airOS):",
			new BMessage(kMsgShareToggled));
		fShare->SetValue(share[0] != '\0' ? B_CONTROL_ON : B_CONTROL_OFF);
		fSharePath = new BTextControl("path", NULL,
			share[0] != '\0' ? share : "~/Android", NULL);
		fChoose = new BButton("choose", "Choose" B_UTF8_ELLIPSIS,
			new BMessage(kMsgShareChoose));
		BStringView* note = new BStringView("note",
			"Changes apply the next time Android starts.");
		BButton* save = new BButton("save", "Save", new BMessage(kMsgSettingsSave));
		BButton* cancel = new BButton("cancel", "Cancel",
			new BMessage(B_QUIT_REQUESTED));
		SetDefaultButton(save);

		BLayoutBuilder::Group<>(this, B_VERTICAL)
			.SetInsets(B_USE_WINDOW_SPACING)
			.AddGrid()
				.AddMenuField(new BMenuField("Memory:", fRam), 0, 0)
				.AddMenuField(new BMenuField("Processors:", fCpus), 0, 1)
				.AddMenuField(new BMenuField("Graphics:", fGpu), 0, 2)
				.AddMenuField(new BMenuField("Screen density:", fDpi), 0, 3)
			.End()
			.Add(fTouch)
			.Add(fShare)
			.AddGroup(B_HORIZONTAL)
				.Add(fSharePath, 1.0)
				.Add(fChoose)
			.End()
			.Add(note)
			.AddGroup(B_HORIZONTAL)
				.AddGlue()
				.Add(cancel)
				.Add(save)
			.End();
		_UpdateShare();
		CenterOnScreen();
	}

	~SettingsWindow()
	{
		delete fPanel;
	}

	virtual void MessageReceived(BMessage* message)
	{
		switch (message->what) {
			case kMsgShareToggled:
				_UpdateShare();
				break;
			case kMsgShareChoose:
				if (fPanel == NULL) {
					fPanel = new BFilePanel(B_OPEN_PANEL, new BMessenger(this),
						NULL, B_DIRECTORY_NODE, false,
						new BMessage(kMsgShareChosen));
					fPanel->Window()->SetTitle("airDroid: Shared folder");
				}
				fPanel->Show();
				break;
			case kMsgShareChosen:
			{
				entry_ref ref;
				if (message->FindRef("refs", &ref) == B_OK) {
					BPath path(&ref);
					fSharePath->SetText(path.Path());
				}
				break;
			}
			case kMsgSettingsSave:
			{
				BString text;
				text << "ram=" << _Value(fRam) << "\n"
					<< "cpus=" << _Value(fCpus) << "\n"
					<< "gpu=" << (_Value(fGpu) == 1 ? "virgl"
						: _Value(fGpu) == 2 ? "none"
						: _Value(fGpu) == 3 ? "venus" : "auto") << "\n"
					<< "dpi=" << _Value(fDpi) << "\n"
					<< "touch=" << (fTouch->Value() == B_CONTROL_ON ? 1 : 0) << "\n"
					<< "share=" << (fShare->Value() == B_CONTROL_ON
						? fSharePath->Text() : "") << "\n";
				AdEvent event = MakeEvent(AD_ACTION, 0);
				event.key = AD_ACT_SAVE_SETTINGS;
				Push(event, text.String());
				Quit();
				break;
			}
			default:
				BWindow::MessageReceived(message);
		}
	}

private:
	static void _AddItem(BPopUpMenu* menu, const char* label, uint32 value,
		bool marked)
	{
		BMessage* m = new BMessage('adVl');
		m->AddUInt32("value", value);
		BMenuItem* item = new BMenuItem(label, m);
		menu->AddItem(item);
		if (marked)
			item->SetMarked(true);
	}

	static uint32 _Value(BPopUpMenu* menu)
	{
		BMenuItem* item = menu->FindMarked();
		uint32 value = 0;
		if (item != NULL && item->Message() != NULL)
			item->Message()->FindUInt32("value", &value);
		return value;
	}

	void _UpdateShare()
	{
		bool on = fShare->Value() == B_CONTROL_ON;
		fSharePath->SetEnabled(on);
		fChoose->SetEnabled(on);
	}

	BPopUpMenu*		fRam;
	BPopUpMenu*		fCpus;
	BPopUpMenu*		fGpu;
	BPopUpMenu*		fDpi;
	BCheckBox*		fTouch;
	BCheckBox*		fShare;
	BTextControl*	fSharePath;
	BButton*		fChoose;
	BFilePanel*		fPanel;
};


// #pragma mark - application


// Set by ad_host_quit(): the application may quit now.
static std::atomic<bool> sQuitAllowed(false);


class AdApp : public BApplication {
public:
	AdApp(const char* signature)
		:
		BApplication(signature),
		fPanel(NULL)
	{
	}

	virtual void ArgvReceived(int32 argc, char** argv)
	{
		std::string text;
		for (int32 i = 1; i < argc; i++) {
			text.append(argv[i]);
			text.push_back('\0');
		}
		Push(MakeEvent(AD_ARGV, 0), text);
	}

	virtual void RefsReceived(BMessage* message)
	{
		entry_ref ref;
		for (int32 i = 0; message->FindRef("refs", i, &ref) == B_OK; i++) {
			BPath path(&ref);
			if (path.InitCheck() == B_OK)
				Push(MakeEvent(AD_REFS, 0), path.Path());
		}
	}

	virtual bool QuitRequested()
	{
		// the engine stops Android first, then quits through ad_host_quit()
		if (sQuitAllowed)
			return true;
		Push(MakeEvent(AD_QUIT, 0));
		return false;
	}

	virtual void MessageReceived(BMessage* message)
	{
		switch (message->what) {
			case B_CLIPBOARD_CHANGED:
			{
				if (be_clipboard->Lock()) {
					BMessage* clip = be_clipboard->Data();
					const char* text = NULL;
					ssize_t len = 0;
					if (clip != NULL && clip->FindData("text/plain", B_MIME_TYPE,
							(const void**)&text, &len) == B_OK && text != NULL) {
						std::string s(text, len);
						if (s != fOwnClipboard)
							Push(MakeEvent(AD_CLIPBOARD, 0), s);
					}
					be_clipboard->Unlock();
				}
				break;
			}
			default:
				BApplication::MessageReceived(message);
		}
	}

	void ShowPanel()
	{
		if (fPanel == NULL) {
			fPanel = new BFilePanel(B_OPEN_PANEL, NULL, NULL, B_FILE_NODE, true);
			fPanel->Window()->SetTitle("airDroid: Install an App (APK or bundle)");
		}
		fPanel->Show();
	}

	std::string fOwnClipboard;

private:
	BFilePanel* fPanel;
};


static AdApp* sApp = NULL;
static MainWindow* sMain = NULL;
static std::mutex sWindowsLock;
static std::map<uint32, AndroidWindow*> sWindows;


static AndroidWindow*
LockWindow(uint32 id)
{
	AndroidWindow* w = NULL;
	{
		std::lock_guard<std::mutex> lock(sWindowsLock);
		auto it = sWindows.find(id);
		if (it == sWindows.end())
			return NULL;
		w = it->second;
	}
	if (!w->Lock())
		return NULL;
	return w;
}


// #pragma mark - icons


/*! Decodes a PNG (any Translation Kit format) into a 32-bit bitmap. */
static BBitmap*
DecodeImage(const uint8* data, size_t length)
{
	if (data == NULL || length == 0)
		return NULL;
	BMemoryIO io(data, length);
	return BTranslationUtils::GetBitmap(&io);
}


/*! Scales a bitmap to size x size B_RGBA32 (box filter). */
static BBitmap*
ScaleIcon(BBitmap* source, int32 size)
{
	BBitmap* out = new BBitmap(BRect(0, 0, size - 1, size - 1), B_RGBA32);
	int32 sw = source->Bounds().IntegerWidth() + 1;
	int32 sh = source->Bounds().IntegerHeight() + 1;
	const uint8* src = (const uint8*)source->Bits();
	int32 sbpr = source->BytesPerRow();
	uint8* dst = (uint8*)out->Bits();
	int32 dbpr = out->BytesPerRow();
	for (int32 y = 0; y < size; y++) {
		for (int32 x = 0; x < size; x++) {
			int32 x0 = x * sw / size, x1 = std::max(x0 + 1, (x + 1) * sw / size);
			int32 y0 = y * sh / size, y1 = std::max(y0 + 1, (y + 1) * sh / size);
			uint32 acc[4] = {0, 0, 0, 0}, n = 0;
			for (int32 yy = y0; yy < y1; yy++) {
				for (int32 xx = x0; xx < x1; xx++) {
					const uint8* p = src + yy * sbpr + xx * 4;
					for (int c = 0; c < 4; c++)
						acc[c] += p[c];
					n++;
				}
			}
			uint8* d = dst + y * dbpr + x * 4;
			for (int c = 0; c < 4; c++)
				d[c] = acc[c] / n;
		}
	}
	return out;
}


/*! Writes a Tracker icon (large + mini, B_CMAP8 from the RGBA image) to a
	node, for Deskbar launchers. */
static void
SetNodeIcon(BNode& node, BBitmap* rgba)
{
	BNodeInfo info(&node);
	const int32 sizes[2] = {32, 16};
	const icon_size which[2] = {B_LARGE_ICON, B_MINI_ICON};
	for (int i = 0; i < 2; i++) {
		BBitmap* scaled = ScaleIcon(rgba, sizes[i]);
		BBitmap cmap(scaled->Bounds(), B_CMAP8);
		// fully transparent pixels become B_TRANSPARENT_MAGIC_CMAP8
		BBitmap flat(scaled->Bounds(), B_RGB32);
		uint8* f = (uint8*)flat.Bits();
		const uint8* s = (const uint8*)scaled->Bits();
		for (int32 p = 0; p < sizes[i] * sizes[i]; p++) {
			f[p * 4 + 0] = s[p * 4 + 0];
			f[p * 4 + 1] = s[p * 4 + 1];
			f[p * 4 + 2] = s[p * 4 + 2];
			f[p * 4 + 3] = 255;
		}
		cmap.ImportBits(&flat);
		uint8* c = (uint8*)cmap.Bits();
		int32 cbpr = cmap.BytesPerRow();
		for (int32 y = 0; y < sizes[i]; y++) {
			for (int32 x = 0; x < sizes[i]; x++) {
				if (s[(y * sizes[i] + x) * 4 + 3] < 128)
					c[y * cbpr + x] = B_TRANSPARENT_MAGIC_CMAP8;
			}
		}
		info.SetIcon(&cmap, which[i]);
		delete scaled;
	}
}


// #pragma mark - audio


/*! One Android audio stream, played with the Media Kit. The guest sends
	only as much as was played (credits), so the ring buffer stays small. */
struct AudioOut {
	BSoundPlayer*		player;
	std::mutex			lock;
	std::vector<uint8>	ring;
	size_t				read;
	size_t				fill;
	uint64				consumed;

	AudioOut() : player(NULL), read(0), fill(0), consumed(0) {}
};


static std::mutex sAudioLock;
static std::map<uint32, AudioOut*> sAudio;


static void
PlayAudio(void* cookie, void* buffer, size_t size,
	const media_raw_audio_format& format)
{
	AudioOut* a = (AudioOut*)cookie;
	uint8* out = (uint8*)buffer;
	std::lock_guard<std::mutex> lock(a->lock);
	size_t n = std::min(size, a->fill);
	size_t cap = a->ring.size();
	for (size_t done = 0; done < n;) {
		size_t chunk = std::min(n - done, cap - a->read);
		memcpy(out + done, &a->ring[a->read], chunk);
		a->read = (a->read + chunk) % cap;
		done += chunk;
	}
	a->fill -= n;
	a->consumed += n;
	if (n < size)
		memset(out + n, 0, size - n);	// underrun: silence
}


// #pragma mark - C API


extern "C" {

int
ad_host_init(const char* signature)
{
	sApp = new AdApp(signature);
	if (sApp->InitCheck() != B_OK)
		return 1;
	return 0;
}


void
ad_host_run(void)
{
	be_clipboard->StartWatching(BMessenger(sApp));
	sApp->Run();
}


void
ad_host_quit(void)
{
	{
		std::lock_guard<std::mutex> lock(sWindowsLock);
		for (auto& w : sWindows) {
			if (w.second->Lock())
				w.second->Quit();
		}
		sWindows.clear();
	}
	if (sMain != NULL && sMain->Lock()) {
		sMain->Quit();
		sMain = NULL;
	}
	// Not Lock() and Quit() from this thread: BApplication::Quit() from
	// another thread posts _QUIT_ and keeps the caller's lock, so the
	// looper could never handle it.
	sQuitAllowed = true;
	BMessenger(sApp).SendMessage(B_QUIT_REQUESTED);
}


int
ad_host_wait(int64 timeoutUs)
{
	std::unique_lock<std::mutex> lock(sQueueLock);
	if (!sQueue.empty() || sWake) {
		sWake = false;
		return 1;
	}
	if (timeoutUs < 0)
		sQueueCond.wait(lock, [] { return !sQueue.empty() || sWake; });
	else {
		sQueueCond.wait_for(lock, std::chrono::microseconds(timeoutUs),
			[] { return !sQueue.empty() || sWake; });
	}
	sWake = false;
	return sQueue.empty() ? 0 : 1;
}


void
ad_host_wake(void)
{
	std::lock_guard<std::mutex> lock(sQueueLock);
	sWake = true;
	sQueueCond.notify_one();
}


int
ad_host_next_event(AdEvent* event)
{
	std::lock_guard<std::mutex> lock(sQueueLock);
	if (sQueue.empty())
		return 0;
	*event = sQueue.front().event;
	sLastText = sQueue.front().text;
	sQueue.pop_front();
	return 1;
}


size_t
ad_host_event_text(char* buffer, size_t length)
{
	std::lock_guard<std::mutex> lock(sQueueLock);
	size_t n = std::min(length, sLastText.size());
	memcpy(buffer, sLastText.data(), n);
	return sLastText.size();
}


/*! x/y < 0: place the window (cascaded); else at that screen position.
	borderless: Android draws the window's caption (multi-window mode). */
void
ad_window_open(uint32 id, const char* title, int32 x, int32 y, int32 width,
	int32 height, int borderless)
{
	BScreen screen;
	BRect frame = screen.Frame();
	if (x < 0 || y < 0) {
		// cascade new windows a little
		float offset = 40 + (id % 8) * 24;
		x = frame.left + offset;
		y = frame.top + offset;
	}
	BRect r(x, y, x + width - 1, y + height - 1);
	AndroidWindow* w = new AndroidWindow(r, title, id, borderless != 0);
	{
		std::lock_guard<std::mutex> lock(sWindowsLock);
		sWindows[id] = w;
	}
	w->Show();
}


void
ad_window_close(uint32 id)
{
	AndroidWindow* w = NULL;
	{
		std::lock_guard<std::mutex> lock(sWindowsLock);
		auto it = sWindows.find(id);
		if (it == sWindows.end())
			return;
		w = it->second;
		sWindows.erase(it);
	}
	if (w->Lock())
		w->Quit();
}


void
ad_window_set_title(uint32 id, const char* title)
{
	AndroidWindow* w = LockWindow(id);
	if (w == NULL)
		return;
	w->SetTitle(title);
	w->Unlock();
}


void
ad_window_resize(uint32 id, int32 width, int32 height)
{
	AndroidWindow* w = LockWindow(id);
	if (w == NULL)
		return;
	w->ResizeTo(width - 1, height - 1);
	w->Unlock();
}


void
ad_window_move(uint32 id, int32 x, int32 y)
{
	AndroidWindow* w = LockWindow(id);
	if (w == NULL)
		return;
	w->MoveTo(x, y);
	w->Unlock();
}


/*! The back bitmap of window \a id for a \a width x \a height frame
	(packed B_RGB32 rows), or NULL; ad_window_frame_end() shows it.
	\a _area: the cloneable area the rows start at, or -1. */
uint32*
ad_window_frame_begin(uint32 id, int32 width, int32 height, int32* _area)
{
	*_area = -1;
	AndroidWindow* w = LockWindow(id);
	if (w == NULL)
		return NULL;
	uint32* bits = w->View()->BeginFrame(width, height, _area);
	w->Unlock();
	return bits;
}


void
ad_window_frame_end(uint32 id)
{
	AndroidWindow* w = LockWindow(id);
	if (w == NULL)
		return;
	w->View()->EndFrame();
	w->Unlock();
}


void
ad_window_present(uint32 id, const uint32* pixels, int32 width, int32 height)
{
	AndroidWindow* w = LockWindow(id);
	if (w == NULL)
		return;
	w->View()->Present(pixels, width, height);
	w->Unlock();
}


void
ad_window_activate(uint32 id)
{
	AndroidWindow* w = LockWindow(id);
	if (w == NULL)
		return;
	w->Activate(true);
	w->Unlock();
}


void
ad_window_hide_cursor(uint32 id, int hidden)
{
	AndroidWindow* w = LockWindow(id);
	if (w == NULL)
		return;
	w->View()->SetCursorHidden(hidden != 0);
	w->Unlock();
}


void
ad_main_show(void)
{
	if (sMain == NULL) {
		sMain = new MainWindow();
		sMain->CenterOnScreen();
		sMain->Show();
		return;
	}
	if (sMain->Lock()) {
		if (sMain->IsHidden())
			sMain->Show();
		sMain->Activate(true);
		sMain->Unlock();
	}
}


void
ad_main_set_status(const char* text, float progress)
{
	if (sMain == NULL || !sMain->Lock())
		return;
	sMain->SetStatus(text, progress);
	sMain->Unlock();
}


/*! The settings window (one at a time). gpu: 0 auto, 1 virgl, 2 none,
	3 venus. */
void
ad_settings_show(uint32 ramMB, uint32 cpus, uint32 gpu, uint32 dpi,
	int touch, const char* share)
{
	static BMessenger sWindow;
	if (sWindow.IsValid()) {
		BMessage activate(B_SET_PROPERTY);
		activate.AddSpecifier("Active");
		activate.AddBool("data", true);
		sWindow.SendMessage(&activate);
		return;
	}
	SettingsWindow* w = new SettingsWindow(ramMB, cpus, gpu, dpi, touch != 0,
		share);
	sWindow = BMessenger(w);
	w->Show();
}


void
ad_main_set_multiwindow(int on)
{
	if (sMain == NULL || !sMain->Lock())
		return;
	sMain->SetMultiWindow(on != 0);
	sMain->Unlock();
}


/*! Replaces the app list; icons are PNG data (may be empty). */
void
ad_main_set_apps(int32 count, const char* const* packages,
	const char* const* names, const uint8* const* icons,
	const size_t* iconLengths)
{
	std::vector<AppEntry> apps;
	for (int32 i = 0; i < count; i++) {
		AppEntry e;
		e.package = packages[i];
		e.name = names[i];
		e.icon = DecodeImage(icons[i], iconLengths[i]);
		apps.push_back(e);
	}
	if (sMain == NULL || !sMain->Lock()) {
		for (auto& a : apps)
			delete a.icon;
		return;
	}
	sMain->SetApps(apps);
	sMain->Unlock();
}


void
ad_open_file_panel(void)
{
	if (sApp->Lock()) {
		sApp->ShowPanel();
		sApp->Unlock();
	}
}


void
ad_alert(const char* title, const char* text)
{
	BAlert* alert = new BAlert(title, text, "OK");
	alert->SetFlags(alert->Flags() | B_CLOSE_ON_ESCAPE);
	alert->Go(NULL);
}


void
ad_clipboard_set(const char* text)
{
	if (!be_clipboard->Lock())
		return;
	be_clipboard->Clear();
	BMessage* clip = be_clipboard->Data();
	clip->AddData("text/plain", B_MIME_TYPE, text, strlen(text));
	sApp->fOwnClipboard = text;
	be_clipboard->Commit();
	be_clipboard->Unlock();
}


/*! A Deskbar/Tracker launcher: an executable script that runs
	"<app> --launch <package>", named after the app, with its icon. */
int
ad_make_launcher(const char* path, const char* appPath, const char* package,
	const uint8* iconPng, size_t iconLength)
{
	BFile file(path, B_WRITE_ONLY | B_CREATE_FILE | B_ERASE_FILE);
	if (file.InitCheck() != B_OK)
		return -1;
	std::string script = "#!/bin/sh\n# airDroid launcher\nexec \"";
	script += appPath;
	script += "\" --launch ";
	script += package;
	script += "\n";
	file.Write(script.data(), script.size());
	chmod(path, 0755);
	BNodeInfo info(&file);
	info.SetType("text/x-script.sh");
	file.WriteAttr("airdroid:package", B_STRING_TYPE, 0, package,
		strlen(package) + 1);
	BBitmap* icon = DecodeImage(iconPng, iconLength);
	if (icon != NULL) {
		SetNodeIcon(file, icon);
		delete icon;
	}
	return 0;
}


/*! Makes sure Tracker knows Android packages (.apk) and app bundles (.apkm,
	.xapk, .apks) and offers airDroid for them (the post-install script does the same at installation). */
void
ad_register_types(const char* signature)
{
	BMimeType apk("application/vnd.android.package-archive");
	// (a type may be there with nothing in it: mimeset makes the types an
	// app says it supports)
	char description[B_MIME_TYPE_LENGTH];
	if (!apk.IsInstalled())
		apk.Install();
	if (apk.GetShortDescription(description) != B_OK || description[0] == '\0') {
		apk.SetShortDescription("Android app");
		apk.SetLongDescription("Android application package");
	}
	// Set every start (a type installed by an older airDroid lacks them).
	// An APK is a zip, and sniffing wins over the extension: this rule
	// outranks zip's (0.40) for the entries Android's build tools put
	// first; the extension catches the rest.
	BMessage extensions;
	extensions.AddString("extensions", "apk");
	apk.SetFileExtensions(&extensions);
	apk.SetSnifferRule("0.60 [0] (\"PK\\003\\004\") [30] (\"AndroidManifest.xml\""
		" | \"META-INF/com/android/build/\" | \"classes.dex\" | \"resources.arsc\")");
	char preferred[B_MIME_TYPE_LENGTH];
	if (apk.GetPreferredApp(preferred) != B_OK || preferred[0] == '\0')
		apk.SetPreferredApp(signature);

	// App bundles: an app as its base APK and split APKs in one zip file
	// (APKMirror's .apkm, APKPure's .xapk, .apks). The registrar goes by a
	// file's contents before its name, and these are zips (an .apkm, signed
	// like a jar, even passes for a Java archive at 0.50): the rule knows
	// what each format starts with. An .apkm has its signature's entries
	// first, the second after the manifest (some kB with many splits); an
	// .xapk starts with "<package>.apk"; .apks is bundletool's (toc.pb) or
	// SAI's (meta.sai_v2.json).
	BMimeType bundle("application/x-vnd.android.app-bundle");
	if (!bundle.IsInstalled())
		bundle.Install();
	if (bundle.GetShortDescription(description) != B_OK || description[0] == '\0') {
		bundle.SetShortDescription("Android app bundle");
		bundle.SetLongDescription("Android app with its split packages");
	}
	BMessage bundleExtensions;
	bundleExtensions.AddString("extensions", "apkm");
	bundleExtensions.AddString("extensions", "xapk");
	bundleExtensions.AddString("extensions", "apks");
	bundle.SetFileExtensions(&bundleExtensions);
	bundle.SetSnifferRule("0.55 [0] (\"PK\\003\\004\") ([60:8192] \"META-INF/APKMIRRO\""
		" | [30:130] \".apk\" | [30] \"split_config.\" | [30] \"toc.pb\""
		" | [30] \"splits/base-\" | [30] \"meta.sai_v\" | [30] \"Android/obb/\")");
	if (bundle.GetPreferredApp(preferred) != B_OK || preferred[0] == '\0')
		bundle.SetPreferredApp(signature);
}


void
ad_audio_open(uint32 id, uint32 rate, uint32 channels)
{
	AudioOut* a = new AudioOut;
	a->ring.resize(rate * channels * 2);	// one second: credits keep it lower
	media_raw_audio_format format = media_raw_audio_format::wildcard;
	format.frame_rate = rate;
	format.channel_count = channels;
	format.format = media_raw_audio_format::B_AUDIO_SHORT;
	format.byte_order = B_MEDIA_LITTLE_ENDIAN;
	format.buffer_size = rate / 100 * channels * 2;	// 10 ms
	a->player = new BSoundPlayer(&format, "airDroid", PlayAudio, NULL, a);
	if (a->player->InitCheck() == B_OK) {
		a->player->Start();
		a->player->SetHasData(true);
	}
	std::lock_guard<std::mutex> lock(sAudioLock);
	sAudio[id] = a;
}


void
ad_audio_write(uint32 id, const uint8* data, size_t length)
{
	AudioOut* a;
	{
		std::lock_guard<std::mutex> lock(sAudioLock);
		auto it = sAudio.find(id);
		if (it == sAudio.end())
			return;
		a = it->second;
	}
	std::lock_guard<std::mutex> lock(a->lock);
	size_t cap = a->ring.size();
	length = std::min(length, cap - a->fill);	// overrun: drop the excess
	size_t write = (a->read + a->fill) % cap;
	for (size_t done = 0; done < length;) {
		size_t chunk = std::min(length - done, cap - write);
		memcpy(&a->ring[write], data + done, chunk);
		write = (write + chunk) % cap;
		done += chunk;
	}
	a->fill += length;
}


/*! Bytes of stream `id` played since the last call. */
uint64
ad_audio_take_consumed(uint32 id)
{
	AudioOut* a;
	{
		std::lock_guard<std::mutex> lock(sAudioLock);
		auto it = sAudio.find(id);
		if (it == sAudio.end())
			return 0;
		a = it->second;
	}
	std::lock_guard<std::mutex> lock(a->lock);
	uint64 n = a->consumed;
	a->consumed = 0;
	return n;
}


void
ad_audio_close(uint32 id)
{
	AudioOut* a;
	{
		std::lock_guard<std::mutex> lock(sAudioLock);
		auto it = sAudio.find(id);
		if (it == sAudio.end())
			return;
		a = it->second;
		sAudio.erase(it);
	}
	a->player->Stop();
	delete a->player;
	delete a;
}


// #pragma mark - microphone


/*! A record stream of Android's: the system's audio input, converted to s16
	interleaved at the stream's rate and channel count (linear
	interpolation), queued for ad_mic_take().

	All streams share one BMediaRecorder (sMicRecorder): Android keeps more
	than one record stream open, and the audio input has one output. A
	recorder per stream connected without an error on real hardware (an
	iMac's hda input) and then none of them got any data. */
struct MicIn {
	uint32				rate;
	uint32				channels;
	std::mutex			lock;
	std::vector<int16>	queue;
	std::vector<float>	previous;	// the last source frame, per output channel
	double				phase;		// next output's position after it, in frames

	MicIn() : rate(0), channels(0), phase(0) {}
};

static std::mutex sMicLock;
static std::map<uint32, MicIn*> sMics;
static BMediaRecorder* sMicRecorder;


static inline float
MicSample(const uint8* frame, uint32 format, uint32 index)
{
	switch (format) {
		case media_raw_audio_format::B_AUDIO_FLOAT:
			return ((const float*)frame)[index];
		case media_raw_audio_format::B_AUDIO_INT:
			return ((const int32*)frame)[index] / 2147483648.0f;
		case media_raw_audio_format::B_AUDIO_SHORT:
			return ((const int16*)frame)[index] / 32768.0f;
		case media_raw_audio_format::B_AUDIO_CHAR:
			return ((const int8*)frame)[index] / 128.0f;
		case media_raw_audio_format::B_AUDIO_UCHAR:
			return (((const uint8*)frame)[index] - 128) / 128.0f;
	}
	return 0;
}


static void
RecordMicFor(MicIn* m, void* data, size_t size, const media_format& format)
{
	const media_raw_audio_format& raw = format.u.raw_audio;
	uint32 sourceChannels = raw.channel_count;
	size_t sampleSize = raw.format & media_raw_audio_format::B_AUDIO_SIZE_MASK;
	if (sourceChannels == 0 || sampleSize == 0 || raw.frame_rate <= 0)
		return;
	size_t frameSize = sampleSize * sourceChannels;
	size_t frames = size / frameSize;
	double step = raw.frame_rate / m->rate;
	std::vector<float> current(m->channels);

	std::lock_guard<std::mutex> lock(m->lock);
	if (m->previous.size() != m->channels)
		m->previous.assign(m->channels, 0);
	for (size_t i = 0; i < frames; i++) {
		const uint8* frame = (const uint8*)data + i * frameSize;
		for (uint32 c = 0; c < m->channels; c++) {
			if (m->channels == 1 && sourceChannels > 1) {
				float sum = 0;
				for (uint32 k = 0; k < sourceChannels; k++)
					sum += MicSample(frame, raw.format, k);
				current[c] = sum / sourceChannels;
			} else
				current[c] = MicSample(frame, raw.format, c % sourceChannels);
		}
		while (m->phase <= 1.0) {
			for (uint32 c = 0; c < m->channels; c++) {
				float v = m->previous[c]
					+ (current[c] - m->previous[c]) * (float)m->phase;
				v = std::max(-1.0f, std::min(1.0f, v));
				m->queue.push_back((int16)(v * 32767));
			}
			m->phase += step;
		}
		m->phase -= 1.0;
		m->previous = current;
	}
	// at most two seconds waiting (nothing takes it: drop the oldest)
	size_t limit = (size_t)m->rate * m->channels * 2;
	if (m->queue.size() > limit)
		m->queue.erase(m->queue.begin(), m->queue.begin() + (m->queue.size() - limit));
}


static void
RecordMic(void*, bigtime_t, void* data, size_t size,
	const media_format& format)
{
	std::lock_guard<std::mutex> lock(sMicLock);
	for (auto& entry : sMics)
		RecordMicFor(entry.second, data, size, format);
}


/*! Starts capturing for record stream `id`; false without an audio input. */
int
ad_mic_open(uint32 id, uint32 rate, uint32 channels)
{
	MicIn* m = new MicIn;
	m->rate = rate;
	m->channels = channels;

	std::lock_guard<std::mutex> lock(sMicLock);
	if (sMicRecorder == NULL) {
		BMediaRecorder* recorder = new BMediaRecorder("airDroid microphone",
			B_MEDIA_RAW_AUDIO);
		media_node input;
		media_format format;
		format.type = B_MEDIA_RAW_AUDIO;
		format.u.raw_audio = media_raw_audio_format::wildcard;
		BMediaRoster* roster = BMediaRoster::Roster();
		status_t status = recorder->InitCheck();
		if (status == B_OK)
			status = roster->GetAudioInput(&input);
		if (status == B_OK)
			status = recorder->SetHooks(RecordMic, NULL, NULL);
		if (status == B_OK) {
			// the input's own format where it says one, else whatever it
			// and the recorder agree on
			media_output output;
			int32 count = 0;
			if (roster->GetFreeOutputsFor(input, &output, 1, &count,
					B_MEDIA_RAW_AUDIO) == B_OK && count >= 1) {
				format.u.raw_audio = output.format.u.raw_audio;
				status = recorder->Connect(input, &output, &format);
			} else {
				recorder->SetAcceptedFormat(format);
				status = recorder->Connect(input, NULL, &format);
			}
		}
		if (status == B_OK)
			status = recorder->Start();
		if (status != B_OK) {
			fprintf(stderr, "airDroid: no microphone (%s)\n", strerror(status));
			delete recorder;
			delete m;
			return 0;
		}
		sMicRecorder = recorder;
	}
	sMics[id] = m;
	return 1;
}


/*! Starts the audio input again. A sound card's node is both the audio
	output and the audio input, and the system mixer stops that node whenever
	nothing plays any more (AudioMixer::_AutoStop()): a recording in progress
	then gets no more buffers. Called when they stay away. */
int
ad_mic_restart()
{
	std::lock_guard<std::mutex> lock(sMicLock);
	if (sMicRecorder == NULL)
		return 0;
	return sMicRecorder->Start(true) == B_OK;
}


/*! Up to `max` captured samples of stream `id`; returns how many. */
size_t
ad_mic_take(uint32 id, int16* out, size_t max)
{
	std::lock_guard<std::mutex> mapLock(sMicLock);
	auto it = sMics.find(id);
	if (it == sMics.end())
		return 0;
	MicIn* m = it->second;
	std::lock_guard<std::mutex> lock(m->lock);
	size_t n = std::min(max, m->queue.size()) / m->channels * m->channels;
	memcpy(out, m->queue.data(), n * sizeof(int16));
	m->queue.erase(m->queue.begin(), m->queue.begin() + n);
	return n;
}


void
ad_mic_close(uint32 id)
{
	MicIn* m;
	BMediaRecorder* recorder = NULL;
	{
		std::lock_guard<std::mutex> lock(sMicLock);
		auto it = sMics.find(id);
		if (it == sMics.end())
			return;
		m = it->second;
		sMics.erase(it);
		if (sMics.empty()) {
			// the last stream: the input is let go of (outside the lock,
			// the hook takes it)
			recorder = sMicRecorder;
			sMicRecorder = NULL;
		}
	}
	if (recorder != NULL) {
		recorder->Stop();
		recorder->Disconnect();
		delete recorder;
	}
	delete m;
}


/*! An Android notification on the desktop. `image` is GLib image-data
	(RGB or RGBA rows) or NULL; then `png` (the app's icon) if any. A click
	starts the app. */
void
ad_notify(uint32 id, const char* app, const char* package, const char* title,
	const char* body, const uint8* image, int32 width, int32 height,
	int32 rowstride, int hasAlpha, const uint8* png, size_t pngLength)
{
	BNotification n(B_INFORMATION_NOTIFICATION);
	n.SetGroup(app);
	n.SetTitle(title);
	n.SetContent(body);
	BString mid;
	mid << "airdroid-" << id;
	n.SetMessageID(mid);
	n.SetOnClickApp("application/x-vnd.airos-airDroid");
	n.AddOnClickArg("--launch");
	n.AddOnClickArg(package);
	BBitmap* icon = NULL;
	if (image != NULL && width > 0 && height > 0) {
		icon = new BBitmap(BRect(0, 0, width - 1, height - 1), B_RGBA32);
		uint8* dst = (uint8*)icon->Bits();
		int32 bpr = icon->BytesPerRow();
		int32 bpp = hasAlpha ? 4 : 3;
		for (int32 y = 0; y < height; y++) {
			const uint8* s = image + (size_t)y * rowstride;
			uint8* d = dst + (size_t)y * bpr;
			for (int32 x = 0; x < width; x++) {
				d[x * 4 + 0] = s[x * bpp + 2];
				d[x * 4 + 1] = s[x * bpp + 1];
				d[x * 4 + 2] = s[x * bpp + 0];
				d[x * 4 + 3] = hasAlpha ? s[x * bpp + 3] : 255;
			}
		}
	} else
		icon = DecodeImage(png, pngLength);
	if (icon != NULL && icon->InitCheck() == B_OK)
		n.SetIcon(icon);
	n.Send();
	delete icon;
}


int
ad_find_directory(int32 which, char* buffer, size_t length)
{
	BPath path;
	if (find_directory((directory_which)which, &path, true) != B_OK)
		return -1;
	strlcpy(buffer, path.Path(), length);
	return 0;
}


/*! The time zone air/OS is set to, as an Olson ID ("Australia/Sydney"). */
int
ad_timezone(char* buffer, size_t length)
{
	BTimeZone zone;
	if (BLocaleRoster::Default()->GetDefaultTimeZone(&zone) != B_OK
		|| zone.ID().IsEmpty())
		return -1;
	strlcpy(buffer, zone.ID().String(), length);
	return 0;
}


/*! The language air/OS is set to, as a BCP 47 tag ("en-AU", "pt-BR",
	"zh-Hans-CN"): the first of the preferred languages, and where that names
	no country, the country of the formatting conventions. */
int
ad_locale(char* buffer, size_t length)
{
	BMessage preferred;
	const char* id;
	if (BLocaleRoster::Default()->GetPreferredLanguages(&preferred) != B_OK
		|| preferred.FindString("language", &id) != B_OK)
		return -1;
	BLanguage language(id);
	const char* code = language.Code();
	if (code == NULL || code[0] == '\0')
		return -1;
	BString tag(code);
	const char* script = language.ScriptCode();
	if (script != NULL && script[0] != '\0')
		tag << "-" << script;
	BString country(language.CountryCode());
	if (country.IsEmpty()) {
		BFormattingConventions conventions;
		if (BLocale::Default()->GetFormattingConventions(&conventions) == B_OK)
			country = conventions.CountryCode();
	}
	if (!country.IsEmpty())
		tag << "-" << country;
	strlcpy(buffer, tag.String(), length);
	return 0;
}


int
ad_app_path(char* buffer, size_t length)
{
	app_info info;
	if (be_app == NULL || be_app->GetAppInfo(&info) != B_OK)
		return -1;
	BPath path(&info.ref);
	strlcpy(buffer, path.Path(), length);
	return 0;
}


float
ad_screen_size(int32* width, int32* height)
{
	BScreen screen;
	BRect frame = screen.Frame();
	*width = frame.IntegerWidth() + 1;
	*height = frame.IntegerHeight() + 1;
	return 0;
}

}	// extern "C"
