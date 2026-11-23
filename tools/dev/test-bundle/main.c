/* The whole app: NativeActivity loads this and calls it. Without the
   library of the device's processor type (a split of its own in a bundle)
   the activity cannot start. */
void
ANativeActivity_onCreate(void* activity, void* savedState, unsigned long size)
{
	(void)activity;
	(void)savedState;
	(void)size;
}
