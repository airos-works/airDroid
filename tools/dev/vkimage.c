/* vkimage: can this driver render into a linear image whose memory is host
 * memory of ours (VK_EXT_external_memory_host)? What a Vulkan app's window
 * buffer on air/OS would be. Also what it says about DRM format modifiers
 * and sync fds, which Android's Venus driver wants from the host.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <OS.h>
#include <vulkan/vulkan.h>

#define W 1280
#define H 720

static void say(const char *what, VkResult r)
{
	printf("%s: %s (%d)\n", what, r == VK_SUCCESS ? "ok" : "FAILED", (int)r);
	fflush(stdout);
	if (r != VK_SUCCESS)
		exit(2);
}

int main(void)
{
	VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
		.pApplicationName = "vkimage", .apiVersion = VK_API_VERSION_1_1 };
	VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
		.pApplicationInfo = &app };
	VkInstance inst;
	say("vkCreateInstance", vkCreateInstance(&ici, NULL, &inst));
	uint32_t n = 1;
	VkPhysicalDevice pd;
	vkEnumeratePhysicalDevices(inst, &n, &pd);
	VkPhysicalDeviceProperties props;
	vkGetPhysicalDeviceProperties(pd, &props);
	printf("device: %s\n", props.deviceName);

	/* sync fds */
	VkPhysicalDeviceExternalFenceInfo fi = {
		.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_FENCE_INFO,
		.handleType = VK_EXTERNAL_FENCE_HANDLE_TYPE_SYNC_FD_BIT };
	VkExternalFenceProperties fp = { .sType = VK_STRUCTURE_TYPE_EXTERNAL_FENCE_PROPERTIES };
	vkGetPhysicalDeviceExternalFenceProperties(pd, &fi, &fp);
	VkPhysicalDeviceExternalSemaphoreInfo si0 = {
		.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_SEMAPHORE_INFO,
		.handleType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT };
	VkExternalSemaphoreProperties sp = { .sType = VK_STRUCTURE_TYPE_EXTERNAL_SEMAPHORE_PROPERTIES };
	vkGetPhysicalDeviceExternalSemaphoreProperties(pd, &si0, &sp);
	printf("sync fd: fence features %#x, semaphore features %#x\n",
		fp.externalFenceFeatures, sp.externalSemaphoreFeatures);

	/* DRM format modifiers of RGBA8 */
	VkDrmFormatModifierPropertiesEXT mods[32];
	VkDrmFormatModifierPropertiesListEXT ml = {
		.sType = VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_EXT,
		.drmFormatModifierCount = 32, .pDrmFormatModifierProperties = mods };
	VkFormatProperties2 fp2 = { .sType = VK_STRUCTURE_TYPE_FORMAT_PROPERTIES_2, .pNext = &ml };
	vkGetPhysicalDeviceFormatProperties2(pd, VK_FORMAT_R8G8B8A8_UNORM, &fp2);
	printf("RGBA8: linear tiling features %#x, optimal %#x, %u modifiers\n",
		fp2.formatProperties.linearTilingFeatures, fp2.formatProperties.optimalTilingFeatures,
		ml.drmFormatModifierCount);
	for (uint32_t i = 0; i < ml.drmFormatModifierCount && i < 32; i++)
		printf("  modifier %#llx: features %#x\n", (unsigned long long)mods[i].drmFormatModifier,
			mods[i].drmFormatModifierTilingFeatures);

	/* a linear color attachment with host memory behind it */
	const VkImageUsageFlags usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT
		| VK_IMAGE_USAGE_TRANSFER_DST_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_SAMPLED_BIT;
	VkPhysicalDeviceExternalImageFormatInfo ext = {
		.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO,
		.handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT };
	VkPhysicalDeviceImageFormatInfo2 ifi = {
		.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2, .pNext = &ext,
		.format = VK_FORMAT_R8G8B8A8_UNORM, .type = VK_IMAGE_TYPE_2D,
		.tiling = VK_IMAGE_TILING_LINEAR, .usage = usage };
	VkExternalImageFormatProperties eip = { .sType = VK_STRUCTURE_TYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES };
	VkImageFormatProperties2 ifp = { .sType = VK_STRUCTURE_TYPE_IMAGE_FORMAT_PROPERTIES_2, .pNext = &eip };
	VkResult r = vkGetPhysicalDeviceImageFormatProperties2(pd, &ifi, &ifp);
	printf("linear RGBA8 color attachment on host memory: %s (%d), external features %#x\n",
		r == VK_SUCCESS ? "supported" : "NOT supported", (int)r,
		eip.externalMemoryProperties.externalMemoryFeatures);
	fflush(stdout);

	float prio = 1.0f;
	VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
		.queueCount = 1, .pQueuePriorities = &prio };
	const char *exts[] = { "VK_EXT_external_memory_host" };
	VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
		.queueCreateInfoCount = 1, .pQueueCreateInfos = &qci,
		.enabledExtensionCount = 1, .ppEnabledExtensionNames = exts };
	VkDevice dev;
	say("vkCreateDevice", vkCreateDevice(pd, &dci, NULL, &dev));
	VkQueue queue;
	vkGetDeviceQueue(dev, 0, 0, &queue);

	VkExternalMemoryImageCreateInfo emi = {
		.sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
		.handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT };
	VkImageCreateInfo ci = { .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, .pNext = &emi,
		.imageType = VK_IMAGE_TYPE_2D, .format = VK_FORMAT_R8G8B8A8_UNORM,
		.extent = { W, H, 1 }, .mipLevels = 1, .arrayLayers = 1,
		.samples = VK_SAMPLE_COUNT_1_BIT, .tiling = VK_IMAGE_TILING_LINEAR, .usage = usage,
		.initialLayout = VK_IMAGE_LAYOUT_UNDEFINED };
	VkImage image;
	say("vkCreateImage (linear)", vkCreateImage(dev, &ci, NULL, &image));
	VkMemoryRequirements mr;
	vkGetImageMemoryRequirements(dev, image, &mr);
	VkImageSubresource sub = { .aspectMask = VK_IMAGE_ASPECT_COLOR_BIT };
	VkSubresourceLayout layout;
	vkGetImageSubresourceLayout(dev, image, &sub, &layout);
	printf("image needs %llu bytes (align %llu, types %#x); layout offset %llu, row pitch %llu (%d * 4 = %d)\n",
		(unsigned long long)mr.size, (unsigned long long)mr.alignment, mr.memoryTypeBits,
		(unsigned long long)layout.offset, (unsigned long long)layout.rowPitch, W, W * 4);

	const size_t size = (mr.size + B_PAGE_SIZE - 1) & ~(size_t)(B_PAGE_SIZE - 1);
	uint32_t *host = NULL;
	area_id area = create_area("vkimage host memory", (void **)&host, B_ANY_ADDRESS, size,
		B_FULL_LOCK, B_READ_AREA | B_WRITE_AREA | B_CLONEABLE_AREA);
	if (area < 0) { printf("create_area failed\n"); return 2; }
	memset(host, 0x11, size);

	PFN_vkGetMemoryHostPointerPropertiesEXT getProps =
		(PFN_vkGetMemoryHostPointerPropertiesEXT)vkGetDeviceProcAddr(dev,
			"vkGetMemoryHostPointerPropertiesEXT");
	VkMemoryHostPointerPropertiesEXT hp = {
		.sType = VK_STRUCTURE_TYPE_MEMORY_HOST_POINTER_PROPERTIES_EXT };
	say("vkGetMemoryHostPointerPropertiesEXT", getProps(dev,
		VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT, host, &hp));
	uint32_t type = 0;
	while (type < 32 && !((hp.memoryTypeBits & mr.memoryTypeBits) & (1u << type)))
		type++;
	printf("host pointer types %#x, common with the image: type %u\n", hp.memoryTypeBits, type);
	if (type == 32) { printf("NO memory type takes both\n"); return 3; }

	VkImportMemoryHostPointerInfoEXT imp = {
		.sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_HOST_POINTER_INFO_EXT,
		.handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT, .pHostPointer = host };
	VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .pNext = &imp,
		.allocationSize = size, .memoryTypeIndex = type };
	VkDeviceMemory mem;
	say("vkAllocateMemory (host pointer)", vkAllocateMemory(dev, &mai, NULL, &mem));
	say("vkBindImageMemory", vkBindImageMemory(dev, image, mem, 0));

	VkImageViewCreateInfo vci = { .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, .image = image,
		.viewType = VK_IMAGE_VIEW_TYPE_2D, .format = VK_FORMAT_R8G8B8A8_UNORM,
		.subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
	VkImageView view;
	say("vkCreateImageView", vkCreateImageView(dev, &vci, NULL, &view));
	VkAttachmentDescription att = { .format = VK_FORMAT_R8G8B8A8_UNORM, .samples = VK_SAMPLE_COUNT_1_BIT,
		.loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR, .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
		.stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE, .stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE,
		.initialLayout = VK_IMAGE_LAYOUT_UNDEFINED, .finalLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL };
	VkAttachmentReference ref = { 0, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL };
	VkSubpassDescription sp0 = { .pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS,
		.colorAttachmentCount = 1, .pColorAttachments = &ref };
	VkRenderPassCreateInfo rpi = { .sType = VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO,
		.attachmentCount = 1, .pAttachments = &att, .subpassCount = 1, .pSubpasses = &sp0 };
	VkRenderPass rp;
	say("vkCreateRenderPass", vkCreateRenderPass(dev, &rpi, NULL, &rp));
	VkFramebufferCreateInfo fbi = { .sType = VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO, .renderPass = rp,
		.attachmentCount = 1, .pAttachments = &view, .width = W, .height = H, .layers = 1 };
	VkFramebuffer fb;
	say("vkCreateFramebuffer", vkCreateFramebuffer(dev, &fbi, NULL, &fb));

	VkCommandPoolCreateInfo pci = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO };
	VkCommandPool pool;
	say("vkCreateCommandPool", vkCreateCommandPool(dev, &pci, NULL, &pool));
	VkCommandBufferAllocateInfo cai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
		.commandPool = pool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1 };
	VkCommandBuffer cb;
	say("vkAllocateCommandBuffers", vkAllocateCommandBuffers(dev, &cai, &cb));

	/* a render pass that clears to opaque (R 0x20, G 0x40, B 0x80), then a
	   clear of a rectangle in it: 0xff804020 as RGBA8 in memory order */
	bigtime_t t0 = system_time();
	VkCommandBufferBeginInfo bi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
	vkBeginCommandBuffer(cb, &bi);
	VkClearValue clear = { .color = { .float32 = { 0x20 / 255.0f, 0x40 / 255.0f, 0x80 / 255.0f, 1.0f } } };
	VkRenderPassBeginInfo rbi = { .sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO, .renderPass = rp,
		.framebuffer = fb, .renderArea = { { 0, 0 }, { W, H } }, .clearValueCount = 1, .pClearValues = &clear };
	vkCmdBeginRenderPass(cb, &rbi, VK_SUBPASS_CONTENTS_INLINE);
	VkClearAttachment ca = { .aspectMask = VK_IMAGE_ASPECT_COLOR_BIT, .colorAttachment = 0,
		.clearValue = { .color = { .float32 = { 1.0f, 0.0f, 0.0f, 1.0f } } } };
	VkClearRect cr = { .rect = { { 100, 50 }, { 200, 100 } }, .baseArrayLayer = 0, .layerCount = 1 };
	vkCmdClearAttachments(cb, 1, &ca, 1, &cr);
	vkCmdEndRenderPass(cb);
	vkEndCommandBuffer(cb);
	VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cb };
	say("vkQueueSubmit (render)", vkQueueSubmit(queue, 1, &si, VK_NULL_HANDLE));
	say("vkQueueWaitIdle", vkQueueWaitIdle(queue));
	bigtime_t t1 = system_time();

	const uint8_t *base = (const uint8_t *)host + layout.offset;
	const uint32_t *corner = (const uint32_t *)base;
	const uint32_t *last = (const uint32_t *)(base + (H - 1) * layout.rowPitch + (W - 1) * 4);
	const uint32_t *inRect = (const uint32_t *)(base + 100 * layout.rowPitch + 200 * 4);
	const uint32_t *outRect = (const uint32_t *)(base + 100 * layout.rowPitch + 310 * 4);
	printf("host memory after rendering: first %#x, last %#x, inside the rectangle %#x, right of it %#x\n",
		*corner, *last, *inRect, *outRect);
	int ok = *corner == 0xff804020 && *last == 0xff804020 && *inRect == 0xff0000ff && *outRect == 0xff804020;
	printf("%s (%lld us for the frame)\n", ok ? "THE GPU RENDERED INTO OUR MEMORY" : "not what was drawn",
		(long long)(t1 - t0));

	/* a few more frames, for the time */
	t0 = system_time();
	for (int i = 0; i < 100; i++) {
		vkQueueSubmit(queue, 1, &si, VK_NULL_HANDLE);
		vkQueueWaitIdle(queue);
	}
	printf("100 more frames: %lld us each\n", (long long)((system_time() - t0) / 100));

	vkDeviceWaitIdle(dev);
	vkDestroyFramebuffer(dev, fb, NULL);
	vkDestroyRenderPass(dev, rp, NULL);
	vkDestroyImageView(dev, view, NULL);
	vkDestroyCommandPool(dev, pool, NULL);
	vkDestroyImage(dev, image, NULL);
	vkFreeMemory(dev, mem, NULL);
	delete_area(area);
	vkDestroyDevice(dev, NULL);
	vkDestroyInstance(inst, NULL);
	printf("done\n");
	return 0;
}
