/* vkprobe STEP [SECONDS]: does what a second Vulkan client does, up to STEP:
 *   enum     instance, physical devices
 *   device   + a device with one queue
 *   submit   + an empty queue submission, waited for
 *   types    + instead of what follows: a fully locked area imported as
 *              each memory type in turn, and mapped where the type allows
 *   hostptr  + 1 MiB of a fully locked area imported as host memory
 *   buffer   + a buffer bound to it, filled by the GPU (vkCmdFillBuffer)
 * then stays for SECONDS (default 5) and tears everything down in order.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/wait.h>
#include <OS.h>
#include <vulkan/vulkan.h>

static void say(const char *what, VkResult r)
{
	printf("%s: %s (%d)\n", what, r == VK_SUCCESS ? "ok" : "FAILED", (int)r);
	fflush(stdout);
	if (r != VK_SUCCESS)
		exit(2);
}

int main(int argc, char **argv)
{
	const char *step = argc > 1 ? argv[1] : "enum";
	int seconds = argc > 2 ? atoi(argv[2]) : 5;
	int level = !strcmp(step, "enum") ? 0 : !strcmp(step, "device") ? 1
		: !strcmp(step, "submit") || !strcmp(step, "types") ? 2
		: !strcmp(step, "hostptr") ? 3 : 4;

	VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
		.pApplicationName = "vkprobe", .apiVersion = VK_API_VERSION_1_1 };
	VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
		.pApplicationInfo = &app };
	VkInstance inst;
	say("vkCreateInstance", vkCreateInstance(&ici, NULL, &inst));

	uint32_t n = 4;
	VkPhysicalDevice pds[4];
	VkResult r = vkEnumeratePhysicalDevices(inst, &n, pds);
	say("vkEnumeratePhysicalDevices", r == VK_INCOMPLETE ? VK_SUCCESS : r);
	if (n == 0) { printf("no device\n"); return 2; }
	VkPhysicalDeviceProperties props;
	vkGetPhysicalDeviceProperties(pds[0], &props);
	printf("device: %s\n", props.deviceName);
	fflush(stdout);

	VkDevice dev = VK_NULL_HANDLE;
	VkQueue queue = VK_NULL_HANDLE;
	VkCommandPool pool = VK_NULL_HANDLE;
	VkDeviceMemory mem = VK_NULL_HANDLE;
	VkBuffer buf = VK_NULL_HANDLE;
	area_id area = -1;
	uint32_t *host = NULL;
	const VkDeviceSize size = 1 << 20;

	if (level >= 1) {
		float prio = 1.0f;
		VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
			.queueFamilyIndex = 0, .queueCount = 1, .pQueuePriorities = &prio };
		const char *exts[] = { "VK_EXT_external_memory_host" };
		VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
			.queueCreateInfoCount = 1, .pQueueCreateInfos = &qci,
			.enabledExtensionCount = level >= 3 || !strcmp(step, "types") ? 1 : 0,
			.ppEnabledExtensionNames = exts };
		say("vkCreateDevice", vkCreateDevice(pds[0], &dci, NULL, &dev));
		vkGetDeviceQueue(dev, 0, 0, &queue);
	}
	if (level >= 2) {
		VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO };
		say("vkQueueSubmit (empty)", vkQueueSubmit(queue, 1, &si, VK_NULL_HANDLE));
		say("vkQueueWaitIdle", vkQueueWaitIdle(queue));
	}
	if (!strcmp(step, "types")) {
		/* what Venus does for memory the guest maps: the guest picks the
		   type, the host imports an area of its own as that type */
		VkPhysicalDeviceMemoryProperties mp;
		vkGetPhysicalDeviceMemoryProperties(pds[0], &mp);
		PFN_vkGetMemoryHostPointerPropertiesEXT getProps =
			(PFN_vkGetMemoryHostPointerPropertiesEXT)vkGetDeviceProcAddr(dev,
				"vkGetMemoryHostPointerPropertiesEXT");
		for (uint32_t type = 0; type < mp.memoryTypeCount; type++) {
			void *memory;
			const size_t length = 65536;
			area_id a = create_area("vkprobe type", &memory, B_ANY_ADDRESS, length,
				B_FULL_LOCK, B_READ_AREA | B_WRITE_AREA | B_CLONEABLE_AREA);
			if (a < 0) { printf("create_area failed\n"); return 2; }
			memset(memory, 0, length);
			VkMemoryHostPointerPropertiesEXT hp = {
				.sType = VK_STRUCTURE_TYPE_MEMORY_HOST_POINTER_PROPERTIES_EXT };
			VkResult r = getProps(dev, VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT,
				memory, &hp);
			VkImportMemoryHostPointerInfoEXT imp = {
				.sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_HOST_POINTER_INFO_EXT,
				.handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT,
				.pHostPointer = memory };
			VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
				.pNext = &imp, .allocationSize = length, .memoryTypeIndex = type };
			VkDeviceMemory m = VK_NULL_HANDLE;
			VkResult ar = r == VK_SUCCESS ? vkAllocateMemory(dev, &mai, NULL, &m) : r;
			VkResult mr = VK_NOT_READY;
			void *mapped = NULL;
			if (ar == VK_SUCCESS
				&& (mp.memoryTypes[type].propertyFlags & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT))
				mr = vkMapMemory(dev, m, 0, length, 0, &mapped);
			printf("type %u: flags %#x heap %u, host pointer %s: import %d, map %d%s\n", type,
				mp.memoryTypes[type].propertyFlags, mp.memoryTypes[type].heapIndex,
				(hp.memoryTypeBits & (1u << type)) ? "allowed" : "NOT allowed", (int)ar, (int)mr,
				mapped != NULL ? (mapped == memory ? " (the area itself)" : " (elsewhere)") : "");
			fflush(stdout);
			if (m != VK_NULL_HANDLE)
				vkFreeMemory(dev, m, NULL);
			printf("type %u: freed\n", type);
			fflush(stdout);
			/* waits until the driver has let go of the pages */
			delete_area(a);
			printf("type %u: area deleted\n", type);
			fflush(stdout);
		}
		fflush(stdout);
		return 0;
	}
	if (level >= 3) {
		area = create_area("vkprobe host memory", (void **)&host, B_ANY_ADDRESS, size,
			B_FULL_LOCK, B_READ_AREA | B_WRITE_AREA | B_CLONEABLE_AREA);
		if (area < 0) { printf("create_area failed\n"); return 2; }
		memset(host, 0, size);
		VkPhysicalDeviceMemoryProperties mp;
		vkGetPhysicalDeviceMemoryProperties(pds[0], &mp);
		VkMemoryHostPointerPropertiesEXT hp = {
			.sType = VK_STRUCTURE_TYPE_MEMORY_HOST_POINTER_PROPERTIES_EXT };
		PFN_vkGetMemoryHostPointerPropertiesEXT getProps =
			(PFN_vkGetMemoryHostPointerPropertiesEXT)vkGetDeviceProcAddr(dev,
				"vkGetMemoryHostPointerPropertiesEXT");
		say("vkGetMemoryHostPointerPropertiesEXT", getProps(dev,
			VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT, host, &hp));
		uint32_t type = 0;
		while (type < mp.memoryTypeCount && !(hp.memoryTypeBits & (1u << type)))
			type++;
		printf("memory type %u of %u (bits %#x)\n", type, mp.memoryTypeCount, hp.memoryTypeBits);
		VkImportMemoryHostPointerInfoEXT imp = {
			.sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_HOST_POINTER_INFO_EXT,
			.handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT,
			.pHostPointer = host };
		VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
			.pNext = &imp, .allocationSize = size, .memoryTypeIndex = type };
		say("vkAllocateMemory (host pointer)", vkAllocateMemory(dev, &mai, NULL, &mem));
	}
	if (level >= 4) {
		VkExternalMemoryBufferCreateInfo ext = {
			.sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_BUFFER_CREATE_INFO,
			.handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT };
		VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .pNext = &ext,
			.size = size, .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT };
		say("vkCreateBuffer", vkCreateBuffer(dev, &bci, NULL, &buf));
		say("vkBindBufferMemory", vkBindBufferMemory(dev, buf, mem, 0));
		VkCommandPoolCreateInfo pci = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO };
		say("vkCreateCommandPool", vkCreateCommandPool(dev, &pci, NULL, &pool));
		VkCommandBufferAllocateInfo cai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
			.commandPool = pool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1 };
		VkCommandBuffer cb;
		say("vkAllocateCommandBuffers", vkAllocateCommandBuffers(dev, &cai, &cb));
		VkCommandBufferBeginInfo bi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
		vkBeginCommandBuffer(cb, &bi);
		vkCmdFillBuffer(cb, buf, 0, size, 0x600DF00D);
		vkEndCommandBuffer(cb);
		VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
			.commandBufferCount = 1, .pCommandBuffers = &cb };
		say("vkQueueSubmit (fill)", vkQueueSubmit(queue, 1, &si, VK_NULL_HANDLE));
		say("vkQueueWaitIdle", vkQueueWaitIdle(queue));
		printf("host memory after the GPU's fill: %#x ... %#x (%s)\n", host[0],
			host[size / 4 - 1], host[0] == 0x600DF00D && host[size / 4 - 1] == 0x600DF00D
				? "the GPU wrote it" : "NOT written");
	}

	if (getenv("VKPROBE_FORK") != NULL && dev != VK_NULL_HANDLE) {
		/* what QEMU does to start the render server: fork with a live
		   device, the child executes a program that uses the GPU */
		unsetenv("VKPROBE_FORK");
		pid_t pid = fork();
		if (pid == 0) {
			execl(argv[0], argv[0], "buffer", "2", (char *)NULL);
			_exit(127);
		}
		printf("forked child %d\n", (int)pid);
		fflush(stdout);
		for (int i = 0; i < 12; i++) {
			VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO };
			VkResult sr = vkQueueSubmit(queue, 1, &si, VK_NULL_HANDLE);
			if (sr == VK_SUCCESS)
				sr = vkQueueWaitIdle(queue);
			if (sr != VK_SUCCESS) {
				printf("parent submit %d while the child runs: FAILED (%d)\n", i, (int)sr);
				fflush(stdout);
				break;
			}
			usleep(300000);
		}
		int status = 0;
		waitpid(pid, &status, 0);
		printf("child exited %d\n", WEXITSTATUS(status));
		VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO };
		say("parent vkQueueSubmit after the child", vkQueueSubmit(queue, 1, &si, VK_NULL_HANDLE));
		say("parent vkQueueWaitIdle after the child", vkQueueWaitIdle(queue));
	}

	printf("holding for %d s\n", seconds);
	fflush(stdout);
	sleep(seconds);

	if (pool) vkDestroyCommandPool(dev, pool, NULL);
	if (buf) vkDestroyBuffer(dev, buf, NULL);
	if (mem) vkFreeMemory(dev, mem, NULL);
	if (area >= 0) delete_area(area);
	if (dev) { vkDeviceWaitIdle(dev); vkDestroyDevice(dev, NULL); }
	vkDestroyInstance(inst, NULL);
	printf("done\n");
	return 0;
}
