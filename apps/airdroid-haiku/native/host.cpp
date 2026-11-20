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
#include <Clipboard.h>
#include <Cursor.h>
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
#include <Menu.h>
#include <MenuBar.h>
#include <MenuItem.h>
#include <Message.h>
#include <MessageRunner.h>
#include <Messenger.h>
#include <Mime.h>
#include <NodeInfo.h>
#include <Notification.h>
#include <String.h>
#include <Path.h>
#include <Region.h>
#include <Roster.h>
#include <Screen.h>
#include <SoundPlayer.h>
#include <ScrollView.h>
#include <StatusBar.h>
#include <StringView.h>
#include <TranslationUtils.h>
#include <View.h>
#include <Window.h>

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
};

static const uint32 kMsgAction = 'adAc';
static const uint32 kMsgAppInvoked = 'adAp';
static const uint32 kMsgRemoveApp = 'adRm';
static const uint32 kMsgPresent = 'adPr';


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
		fCursorHidden(false)
	{
		SetViewColor(B_TRANSPARENT_COLOR);
		SetDrawingMode(B_OP_COPY);
	}

	~AndroidView()
	{
		delete fBitmap;
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
		if (fBitmap != NULL) {
			painted = fBitmap->Bounds() & updateRect;
			if (painted.IsValid())
				DrawBitmapAsync(fBitmap, painted, painted);
		}
		SetHighColor(0, 0, 0);
		BRegion rest(updateRect);
		if (painted.IsValid())
			rest.Exclude(painted);
		FillRegion(&rest);
	}

	virtual void MouseDown(BPoint where)
	{
		MakeFocus(true);
		SetMouseEventMask(B_POINTER_EVENTS, B_NO_POINTER_HISTORY);
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
		if (transit == B_EXITED_VIEW) {
			Push(MakeEvent(AD_MOUSE_EXITED, fId));
			return;
		}
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
	bool		fCursorHidden;
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
		file->AddItem(new BMenuItem("Install APK" B_UTF8_ELLIPSIS,
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
		BButton* install = new BButton("install", "Install APK" B_UTF8_ELLIPSIS,
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


// #pragma mark - application


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
			fPanel->Window()->SetTitle("airDroid: Install APK");
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
	BMessenger(sApp).SendMessage(B_QUIT_REQUESTED);
	// AdApp::QuitRequested refuses: quit the looper directly
	if (sApp->Lock())
		sApp->Quit();
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


/*! Makes sure Tracker knows Android packages (.apk) and offers airDroid
	for them (the post-install script does the same at installation). */
void
ad_register_types(const char* signature)
{
	BMimeType apk("application/vnd.android.package-archive");
	if (!apk.IsInstalled()) {
		apk.Install();
		apk.SetShortDescription("Android app");
		apk.SetLongDescription("Android application package");
		BMessage extensions;
		extensions.AddString("extensions", "apk");
		apk.SetFileExtensions(&extensions);
	}
	char preferred[B_MIME_TYPE_LENGTH];
	if (apk.GetPreferredApp(preferred) != B_OK || preferred[0] == '\0')
		apk.SetPreferredApp(signature);
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
