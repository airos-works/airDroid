/* Drops files on a window of a running application, as Tracker does when
   they are dragged there: sends B_SIMPLE_DATA with their refs to the window.

     dropfile SIGNATURE WINDOW-INDEX FILE...

   Build: tools/haiku/haiku-c++ tools/dev/dropfile.cpp -o dropfile -lbe */
#include <stdio.h>
#include <stdlib.h>

#include <Application.h>
#include <Entry.h>
#include <Message.h>
#include <Messenger.h>


int
main(int argc, char** argv)
{
	if (argc < 4) {
		fprintf(stderr, "usage: %s SIGNATURE WINDOW-INDEX FILE...\n", argv[0]);
		return 2;
	}
	BMessenger app(argv[1]);
	if (!app.IsValid()) {
		fprintf(stderr, "%s is not running\n", argv[1]);
		return 1;
	}
	BMessage ask(B_GET_PROPERTY);
	ask.AddSpecifier("Messenger");
	ask.AddSpecifier("Window", (int32)atoi(argv[2]));
	BMessage reply;
	BMessenger window;
	if (app.SendMessage(&ask, &reply, 2000000, 2000000) != B_OK
		|| reply.FindMessenger("result", &window) != B_OK) {
		fprintf(stderr, "no window %s\n", argv[2]);
		return 1;
	}
	BMessage drop(B_SIMPLE_DATA);
	for (int i = 3; i < argc; i++) {
		entry_ref ref;
		if (get_ref_for_path(argv[i], &ref) != B_OK) {
			fprintf(stderr, "%s: no such file\n", argv[i]);
			return 1;
		}
		drop.AddRef("refs", &ref);
	}
	return window.SendMessage(&drop) == B_OK ? 0 : 1;
}
