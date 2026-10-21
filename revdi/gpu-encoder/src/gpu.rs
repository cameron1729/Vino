// SPDX-License-Identifier: GPL-2.0-or-later
use anyhow::{anyhow, bail, ensure, Context, Result};
use ash::{vk, Entry};
use serde::Serialize;
use std::{
    ffi::{c_void, CStr},
    io::Cursor,
    ptr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};
use vino_chimera::{
    capture_pool::{CapturePool, CaptureStorage},
    kvino,
    scanout::StripEncoder,
    surface::{CursorBitmap, NativeSurface},
};
mod comparison;
pub mod shared_capture;

const OUTPUT_WORDS: usize = 2048;
// Shared coefficients, quadrant reductions, last indices and stream offsets.
const ENCODE_SHARED_BYTES: u32 = (64 * 17 + 64 * 3 * 2 + 48 + 16 * 2 + 48 * 4 + 3) * 4;
// 48 AC planes * 63 coefficients * 20 bits, plus at most 16*39 sync and
// 48*20 DC bits, header and region padding, fit in the 8192-byte slot.
const SHADERS: [&[u8]; 1] = [include_bytes!(concat!(env!("OUT_DIR"), "/encode.spv"))];

#[derive(Clone, Debug, Serialize)]
pub struct DeviceInfo {
    pub index: usize,
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub device_type: String,
    pub api_version: u32,
    pub driver_version: u32,
    pub device_uuid: String,
}

struct Instance {
    _entry: Entry,
    raw: ash::Instance,
    debug: Option<(ash::ext::debug_utils::Instance, vk::DebugUtilsMessengerEXT)>,
    errors: Box<AtomicUsize>,
}
unsafe extern "system" fn debug_callback(
    severity: vk::DebugUtilsMessageSeverityFlagsEXT,
    _kind: vk::DebugUtilsMessageTypeFlagsEXT,
    data: *const vk::DebugUtilsMessengerCallbackDataEXT<'_>,
    user: *mut c_void,
) -> vk::Bool32 {
    if severity.contains(vk::DebugUtilsMessageSeverityFlagsEXT::ERROR) {
        (*(user as *const AtomicUsize)).fetch_add(1, Ordering::Relaxed);
    }
    if !data.is_null() && !(*data).p_message.is_null() {
        eprintln!(
            "Vulkan: {}",
            CStr::from_ptr((*data).p_message).to_string_lossy()
        );
    }
    vk::FALSE
}
impl Instance {
    fn new(validation: bool) -> Result<Self> {
        unsafe {
            let entry = Entry::load().context("loading the Vulkan loader")?;
            let errors = Box::new(AtomicUsize::new(0));
            let layer = c"VK_LAYER_KHRONOS_validation";
            if validation {
                ensure!(
                    entry
                        .enumerate_instance_layer_properties()?
                        .iter()
                        .any(|p| CStr::from_ptr(p.layer_name.as_ptr()) == layer),
                    "Vulkan validation layer is not installed"
                );
            }
            let layers = if validation {
                vec![layer.as_ptr()]
            } else {
                vec![]
            };
            let extensions = if validation {
                vec![ash::ext::debug_utils::NAME.as_ptr()]
            } else {
                vec![]
            };
            let app = vk::ApplicationInfo::default()
                .application_name(c"vino-gpu")
                .api_version(vk::API_VERSION_1_2);
            let mut debug_info = vk::DebugUtilsMessengerCreateInfoEXT::default()
                .message_severity(
                    vk::DebugUtilsMessageSeverityFlagsEXT::ERROR
                        | vk::DebugUtilsMessageSeverityFlagsEXT::WARNING,
                )
                .message_type(
                    vk::DebugUtilsMessageTypeFlagsEXT::GENERAL
                        | vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION
                        | vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE,
                )
                .pfn_user_callback(Some(debug_callback))
                .user_data((&*errors as *const AtomicUsize).cast_mut().cast());
            let mut create = vk::InstanceCreateInfo::default()
                .application_info(&app)
                .enabled_layer_names(&layers)
                .enabled_extension_names(&extensions);
            if validation {
                create = create.push_next(&mut debug_info);
            }
            let raw = entry.create_instance(&create, None)?;
            let mut result = Self {
                _entry: entry,
                raw,
                debug: None,
                errors,
            };
            if validation {
                let loader = ash::ext::debug_utils::Instance::new(&result._entry, &result.raw);
                let messenger = loader.create_debug_utils_messenger(&debug_info, None)?;
                result.debug = Some((loader, messenger));
            }
            Ok(result)
        }
    }
    fn devices(&self) -> Result<Vec<(vk::PhysicalDevice, DeviceInfo)>> {
        unsafe {
            Ok(self
                .raw
                .enumerate_physical_devices()?
                .into_iter()
                .enumerate()
                .map(|(index, handle)| {
                    let mut id = vk::PhysicalDeviceIDProperties::default();
                    let mut properties =
                        vk::PhysicalDeviceProperties2::default().push_next(&mut id);
                    self.raw
                        .get_physical_device_properties2(handle, &mut properties);
                    let p = properties.properties;
                    (
                        handle,
                        DeviceInfo {
                            index,
                            name: CStr::from_ptr(p.device_name.as_ptr())
                                .to_string_lossy()
                                .into_owned(),
                            vendor_id: p.vendor_id,
                            device_id: p.device_id,
                            device_type: format!("{:?}", p.device_type),
                            api_version: p.api_version,
                            driver_version: p.driver_version,
                            device_uuid: id
                                .device_uuid
                                .iter()
                                .map(|v| format!("{v:02x}"))
                                .collect(),
                        },
                    )
                })
                .collect())
        }
    }
}
impl Drop for Instance {
    fn drop(&mut self) {
        unsafe {
            if let Some((loader, messenger)) = &self.debug {
                loader.destroy_debug_utils_messenger(*messenger, None);
            }
            self.raw.destroy_instance(None);
        }
    }
}
pub fn devices() -> Result<Vec<DeviceInfo>> {
    Ok(Instance::new(false)?
        .devices()?
        .into_iter()
        .map(|(_, info)| info)
        .collect())
}

struct DeviceContext {
    instance: Instance,
    device: ash::Device,
    queue: vk::Queue,
    memory: vk::PhysicalDeviceMemoryProperties,
    limits: vk::PhysicalDeviceLimits,
    family: u32,
    physical: vk::PhysicalDevice,
    external_memory_fd: bool,
    queue_lock: Mutex<()>,
}
impl DeviceContext {
    fn wait_idle(&self) -> std::result::Result<(), vk::Result> {
        let _queue = self.queue_lock.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { self.device.device_wait_idle() }
    }
}
impl Drop for DeviceContext {
    fn drop(&mut self) {
        unsafe {
            let _ = self.wait_idle();
            self.device.destroy_device(None);
        }
    }
}
struct Buffer {
    ctx: Arc<DeviceContext>,
    raw: vk::Buffer,
    memory: vk::DeviceMemory,
    mapped: *mut u8,
    size: usize,
    host_cached: bool,
}
impl Buffer {
    fn new(ctx: Arc<DeviceContext>, size: usize, host: bool) -> Result<Self> {
        ensure!(
            size > 0 && size as u64 <= u64::from(ctx.limits.max_storage_buffer_range),
            "storage buffer too large: {size}"
        );
        let mut result = Self {
            ctx,
            raw: vk::Buffer::null(),
            memory: vk::DeviceMemory::null(),
            mapped: ptr::null_mut(),
            size,
            host_cached: false,
        };
        unsafe {
            let device = &result.ctx.device;
            result.raw = device.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size as u64)
                    .usage(
                        vk::BufferUsageFlags::STORAGE_BUFFER
                            | vk::BufferUsageFlags::TRANSFER_DST
                            | vk::BufferUsageFlags::TRANSFER_SRC,
                    )
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )?;
            let req = device.get_buffer_memory_requirements(result.raw);
            let required = if host {
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT
            } else {
                vk::MemoryPropertyFlags::DEVICE_LOCAL
            };
            let index = (0..result.ctx.memory.memory_type_count)
                .filter(|&i| {
                    req.memory_type_bits & (1 << i) != 0
                        && result.ctx.memory.memory_types[i as usize]
                            .property_flags
                            .contains(required)
                        && !result.ctx.memory.memory_types[i as usize]
                            .property_flags
                            .intersects(
                                vk::MemoryPropertyFlags::DEVICE_COHERENT_AMD
                                    | vk::MemoryPropertyFlags::PROTECTED
                                    | vk::MemoryPropertyFlags::LAZILY_ALLOCATED,
                            )
                })
                .max_by_key(|&i| {
                    let flags = result.ctx.memory.memory_types[i as usize].property_flags;
                    u8::from(flags.contains(vk::MemoryPropertyFlags::HOST_CACHED)) * 2
                        + u8::from(flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL))
                })
                .context("no compatible Vulkan memory type")?;
            result.host_cached = result.ctx.memory.memory_types[index as usize]
                .property_flags
                .contains(vk::MemoryPropertyFlags::HOST_CACHED);
            result.memory = device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size)
                    .memory_type_index(index),
                None,
            )?;
            device.bind_buffer_memory(result.raw, result.memory, 0)?;
            if host {
                result.mapped = device
                    .map_memory(
                        result.memory,
                        0,
                        vk::WHOLE_SIZE,
                        vk::MemoryMapFlags::empty(),
                    )?
                    .cast();
            }
        }
        Ok(result)
    }
    unsafe fn write(&self, bytes: &[u8]) {
        assert!(bytes.len() <= self.size && !self.mapped.is_null());
        ptr::copy_nonoverlapping(bytes.as_ptr(), self.mapped, bytes.len());
    }
    /// The transform reads only the requested 64x16 strips. For sparse updates,
    /// copy their rows rather than uploading the whole desktop. Coordinates have
    /// already been validated; adjacent requests are coalesced when possible.
    unsafe fn write_pixels(&self, width: usize, rgb: &[u8], coords: &[(usize, usize)]) -> usize {
        assert!(rgb.len() <= self.size && !self.mapped.is_null());
        if coords.len() * (64 * 16 * 3) >= rgb.len() / 4 {
            self.write(rgb);
            return rgb.len();
        }
        let row = width * 3;
        let mut first = 0;
        while first < coords.len() {
            let (x, y) = coords[first];
            let mut end = first + 1;
            while end < coords.len() && coords[end] == (x + (end - first) * 64, y) {
                end += 1;
            }
            let bytes = (end - first) * 64 * 3;
            for dy in 0..16 {
                let offset = (y + dy) * row + x * 3;
                ptr::copy_nonoverlapping(rgb.as_ptr().add(offset), self.mapped.add(offset), bytes);
            }
            first = end;
        }
        coords.len() * 64 * 16 * 3
    }
}
impl Drop for Buffer {
    fn drop(&mut self) {
        unsafe {
            if !self.mapped.is_null() {
                self.ctx.device.unmap_memory(self.memory);
            }
            if self.raw != vk::Buffer::null() {
                self.ctx.device.destroy_buffer(self.raw, None);
            }
            if self.memory != vk::DeviceMemory::null() {
                self.ctx.device.free_memory(self.memory, None);
            }
        }
    }
}

/// The only transferable Vulkan allocation. Its pointer is accessed exclusively
/// through CapturePool's writer/read leases; GPU submissions borrow those same
/// read leases until their fence has completed. No command pool, queue or
/// mutable encoder state is transferred to the capture thread.
struct CaptureAllocation(Buffer);
// SAFETY: Buffer ownership keeps its mapped allocation and DeviceContext alive.
// The unsafe CaptureStorage contract requires a pool to serialize writes and
// retain immutable read leases for CPU/GPU readers. Vulkan destruction happens
// only after all owners release this allocation; it owns distinct resources.
unsafe impl Send for CaptureAllocation {}
unsafe impl Sync for CaptureAllocation {}
unsafe impl CaptureStorage for CaptureAllocation {
    fn mapped_ptr(&self) -> *mut u8 {
        self.0.mapped
    }
    fn len(&self) -> usize {
        self.0.size
    }
}

struct CaptureBuffers {
    pool_id: u64,
    width: usize,
    visible_height: usize,
    padded_height: usize,
    stride: usize,
    allocations: Vec<Arc<CaptureAllocation>>,
}

/// Cumulative stage times; GPU timestamps exclude CPU submission and readback.
/// These diagnostics are part of the measured encoder path.
#[derive(Clone, Copy, Default, Serialize)]
pub struct StageTimes {
    pub upload_ms: f64,
    // Retained for readers of earlier split-pipeline reports; zero for fused runs.
    pub transform_ms: f64,
    pub pack_ms: f64,
    pub fused_ms: f64,
    pub submit_wait_ms: f64,
    pub readback_ms: f64,
    /// Desktop bytes copied into the encoder's separate RGB input allocation.
    pub pixel_upload_bytes: u64,
    pub cursor_upload_bytes: u64,
    pub direct_dispatches: u64,
    /// Native change detection has its own submission and never counts as an encode.
    pub compare_dispatches: u64,
    pub compare_gpu_ms: f64,
    pub compare_submit_wait_ms: f64,
    pub compare_upload_ms: f64,
    pub compare_readback_ms: f64,
    pub compare_cursor_upload_bytes: u64,
}
impl StageTimes {
    pub fn since(self, before: Self) -> Self {
        Self {
            upload_ms: self.upload_ms - before.upload_ms,
            transform_ms: self.transform_ms - before.transform_ms,
            pack_ms: self.pack_ms - before.pack_ms,
            fused_ms: self.fused_ms - before.fused_ms,
            submit_wait_ms: self.submit_wait_ms - before.submit_wait_ms,
            readback_ms: self.readback_ms - before.readback_ms,
            pixel_upload_bytes: self.pixel_upload_bytes - before.pixel_upload_bytes,
            cursor_upload_bytes: self.cursor_upload_bytes - before.cursor_upload_bytes,
            direct_dispatches: self.direct_dispatches - before.direct_dispatches,
            compare_dispatches: self.compare_dispatches - before.compare_dispatches,
            compare_gpu_ms: self.compare_gpu_ms - before.compare_gpu_ms,
            compare_submit_wait_ms: self.compare_submit_wait_ms - before.compare_submit_wait_ms,
            compare_upload_ms: self.compare_upload_ms - before.compare_upload_ms,
            compare_readback_ms: self.compare_readback_ms - before.compare_readback_ms,
            compare_cursor_upload_bytes: self.compare_cursor_upload_bytes
                - before.compare_cursor_upload_bytes,
        }
    }
}
pub struct GpuEncoder {
    ctx: Arc<DeviceContext>,
    pub info: DeviceInfo,
    layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    descriptor_pool: vk::DescriptorPool,
    set: vk::DescriptorSet,
    command_pool: vk::CommandPool,
    command: vk::CommandBuffer,
    fence: vk::Fence,
    queries: vk::QueryPool,
    timestamp_bits: u32,
    pub stage_times: StageTimes,
    pipelines: Vec<vk::Pipeline>,
    buffers: Vec<Buffer>,
    readback: Option<Buffer>,
    pub dispatches: u64,
    pub encoded_strips: u64,
    output_words: usize,
    capture_buffers: Option<CaptureBuffers>,
    cursor_bitmap: Option<CursorBitmap>,
    comparison: Option<comparison::Comparison>,
}
impl GpuEncoder {
    pub fn new(index: Option<usize>, validation: bool) -> Result<Self> {
        let instance = Instance::new(validation)?;
        let available = instance.devices()?;
        let (physical, info) = available
            .into_iter()
            .find(|(_, d)| {
                index.map_or(d.vendor_id == 0x1002 && d.name.contains("780M"), |i| {
                    d.index == i
                })
            })
            .context("requested GPU unavailable; use 'vino-gpu devices' and --device")?;
        ensure!(
            info.device_type == "INTEGRATED_GPU" || info.device_type == "DISCRETE_GPU",
            "software/virtual Vulkan devices are not accepted: {}",
            info.name
        );
        ensure!(
            info.api_version >= vk::API_VERSION_1_2,
            "Vulkan 1.2 is required"
        );
        let (ctx, timestamp_bits) = unsafe {
            let queues = instance
                .raw
                .get_physical_device_queue_family_properties(physical);
            let family = queues
                .iter()
                .enumerate()
                .filter(|(_, q)| q.queue_flags.contains(vk::QueueFlags::COMPUTE))
                .min_by_key(|(_, q)| u8::from(q.queue_flags.contains(vk::QueueFlags::GRAPHICS)))
                .map(|(i, _)| i as u32)
                .context("no compute queue")?;
            let limits = instance.raw.get_physical_device_properties(physical).limits;
            ensure!(
                limits.max_per_stage_descriptor_storage_buffers >= 5
                    && limits.max_descriptor_set_storage_buffers >= 5
                    && limits.max_compute_work_group_invocations >= 64
                    && limits.max_compute_work_group_size[0] >= 64
                    && limits.max_compute_shared_memory_size >= ENCODE_SHARED_BYTES,
                "GPU limits cannot support five storage buffers, a 64-lane workgroup and 6988 shared bytes"
            );
            let priorities = [1.0];
            let queue_info = [vk::DeviceQueueCreateInfo::default()
                .queue_family_index(family)
                .queue_priorities(&priorities)];
            let extensions = instance
                .raw
                .enumerate_device_extension_properties(physical)?;
            let external_memory_fd = [
                ash::khr::external_memory_fd::NAME,
                ash::ext::external_memory_dma_buf::NAME,
                ash::ext::queue_family_foreign::NAME,
            ]
            .iter()
            .all(|name| {
                extensions
                    .iter()
                    .any(|extension| CStr::from_ptr(extension.extension_name.as_ptr()) == *name)
            });
            let enabled_extensions: Vec<_> = if external_memory_fd {
                vec![
                    ash::khr::external_memory_fd::NAME.as_ptr(),
                    ash::ext::external_memory_dma_buf::NAME.as_ptr(),
                    ash::ext::queue_family_foreign::NAME.as_ptr(),
                ]
            } else {
                Vec::new()
            };
            let device = instance.raw.create_device(
                physical,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&queue_info)
                    .enabled_extension_names(&enabled_extensions),
                None,
            )?;
            let queue = device.get_device_queue(family, 0);
            let memory = instance.raw.get_physical_device_memory_properties(physical);
            (
                Arc::new(DeviceContext {
                    instance,
                    device,
                    queue,
                    memory,
                    limits,
                    family,
                    physical,
                    external_memory_fd,
                    queue_lock: Mutex::new(()),
                }),
                queues[family as usize].timestamp_valid_bits,
            )
        };
        let mut result = Self {
            ctx,
            info,
            layout: vk::DescriptorSetLayout::null(),
            pipeline_layout: vk::PipelineLayout::null(),
            descriptor_pool: vk::DescriptorPool::null(),
            set: vk::DescriptorSet::null(),
            command_pool: vk::CommandPool::null(),
            command: vk::CommandBuffer::null(),
            fence: vk::Fence::null(),
            pipelines: vec![],
            buffers: vec![],
            readback: None,
            queries: vk::QueryPool::null(),
            timestamp_bits,
            stage_times: StageTimes::default(),
            dispatches: 0,
            encoded_strips: 0,
            output_words: OUTPUT_WORDS,
            capture_buffers: None,
            cursor_bitmap: None,
            comparison: None,
        };
        unsafe {
            let d = &result.ctx.device;
            let bindings: Vec<_> = (0..5)
                .map(|i| {
                    vk::DescriptorSetLayoutBinding::default()
                        .binding(i)
                        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                        .descriptor_count(1)
                        .stage_flags(vk::ShaderStageFlags::COMPUTE)
                })
                .collect();
            result.layout = d.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )?;
            let layouts = [result.layout];
            let ranges = [vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
                .offset(0)
                .size(48)];
            result.pipeline_layout = d.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&layouts)
                    .push_constant_ranges(&ranges),
                None,
            )?;
            let sizes = [vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(5)];
            result.descriptor_pool = d.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(1)
                    .pool_sizes(&sizes),
                None,
            )?;
            result.set = d.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(result.descriptor_pool)
                    .set_layouts(&layouts),
            )?[0];
            result.command_pool = d.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(result.ctx.family)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )?;
            result.command = d.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(result.command_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )?[0];
            result.fence = d.create_fence(&vk::FenceCreateInfo::default(), None)?;
            if timestamp_bits > 0 {
                result.queries = d.create_query_pool(
                    &vk::QueryPoolCreateInfo::default()
                        .query_type(vk::QueryType::TIMESTAMP)
                        .query_count(2),
                    None,
                )?;
            }
            for source in SHADERS {
                let words = ash::util::read_spv(&mut Cursor::new(source))?;
                let module = d.create_shader_module(
                    &vk::ShaderModuleCreateInfo::default().code(&words),
                    None,
                )?;
                let stage = vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::COMPUTE)
                    .module(module)
                    .name(c"main");
                let pipeline = d.create_compute_pipelines(
                    vk::PipelineCache::null(),
                    &[vk::ComputePipelineCreateInfo::default()
                        .stage(stage)
                        .layout(result.pipeline_layout)],
                    None,
                );
                d.destroy_shader_module(module, None);
                match pipeline {
                    Ok(mut pipelines) => result.pipelines.append(&mut pipelines),
                    Err((pipelines, error)) => {
                        result.pipelines.extend(pipelines);
                        return Err(anyhow!("creating compute pipeline: {error:?}"));
                    }
                }
            }
        }
        Ok(result)
    }
    pub fn has_timestamps(&self) -> bool {
        self.timestamp_bits > 0
    }
    pub fn validation_errors(&self) -> usize {
        self.ctx.instance.errors.load(Ordering::Relaxed)
    }
    /// Compare immutable native capture slots exactly on the GPU. Output flags
    /// are row-major 64x16 strips; this never uploads desktop pixels. The caller
    /// keeps both originating leases alive through this synchronous fence wait.
    pub fn compare_native(
        &mut self,
        previous: NativeSurface<'_>,
        current: NativeSurface<'_>,
        flags: &mut [bool],
    ) -> Result<()> {
        ensure!(
            previous.width() == current.width() && previous.height() == current.height(),
            "native comparison dimensions differ"
        );
        ensure!(
            current.width().is_multiple_of(64)
                && current.height().is_multiple_of(16)
                && flags.len() == current.width() / 64 * (current.height() / 16),
            "native comparison flags do not match the 64x16 strip grid"
        );
        let inputs = [previous, current].map(|surface| {
            self.native_input(surface).map(|buffer| {
                vk::DescriptorBufferInfo::default()
                    .buffer(buffer.raw)
                    .range(buffer.size as u64)
            })
        });
        let [previous_input, current_input] = inputs;
        let inputs = [previous_input?, current_input?];
        if self.comparison.is_none() {
            self.comparison = Some(comparison::Comparison::new(
                self.ctx.clone(),
                self.timestamp_bits,
            )?);
        }
        let stages = self
            .comparison
            .as_mut()
            .unwrap()
            .run(previous, current, inputs, flags)?;
        self.stage_times.compare_dispatches += stages.compare_dispatches;
        self.stage_times.compare_gpu_ms += stages.compare_gpu_ms;
        self.stage_times.compare_submit_wait_ms += stages.compare_submit_wait_ms;
        self.stage_times.compare_upload_ms += stages.compare_upload_ms;
        self.stage_times.compare_readback_ms += stages.compare_readback_ms;
        self.stage_times.compare_cursor_upload_bytes += stages.compare_cursor_upload_bytes;
        ensure!(
            self.validation_errors() == 0,
            "Vulkan validation reported an error"
        );
        Ok(())
    }
    /// Exercise a real shader overflow path without making a smaller memory allocation.
    pub fn set_output_limit(&mut self, bytes: usize) -> Result<()> {
        ensure!(
            (16..=OUTPUT_WORDS * 4).contains(&bytes) && bytes.is_multiple_of(4),
            "output limit must be a multiple of four in 16..8192"
        );
        self.output_words = bytes / 4;
        Ok(())
    }
    /// Allocate the four stable, mapped input slots before any display takeover.
    /// EVDI registers only visible_height rows; the shader synthesizes black
    /// pixels in the extra rows needed by the 16-row encoder strip layout.
    pub fn create_capture_pool(
        &mut self,
        width: usize,
        visible_height: usize,
        padded_height: usize,
    ) -> Result<Arc<CapturePool>> {
        let stride = width.checked_mul(4).context("capture stride overflow")?;
        self.create_capture_pool_with_stride(width, visible_height, padded_height, stride)
    }

    pub(crate) fn create_capture_pool_with_stride(
        &mut self,
        width: usize,
        visible_height: usize,
        padded_height: usize,
        stride: usize,
    ) -> Result<Arc<CapturePool>> {
        ensure!(
            self.info.vendor_id == 0x1002 && self.info.device_type == "INTEGRATED_GPU",
            "direct capture currently requires an integrated Radeon GPU"
        );
        ensure!(
            width > 0
                && width <= 65535
                && width.is_multiple_of(64)
                && visible_height > 0
                && padded_height <= 65535
                && padded_height == visible_height.div_ceil(16) * 16
                && stride >= width * 4
                && stride.is_multiple_of(4)
                && stride <= u32::MAX as usize,
            "invalid direct capture dimensions or stride"
        );
        let size = stride
            .checked_mul(padded_height)
            .context("capture allocation size overflow")?;
        let mut allocations = Vec::with_capacity(4);
        for _ in 0..4 {
            let buffer = Buffer::new(self.ctx.clone(), size, true)?;
            ensure!(
                buffer.host_cached,
                "direct capture requires cached, coherent host-visible Vulkan memory"
            );
            // No other owners or GPU readers exist until the pool is published.
            unsafe { ptr::write_bytes(buffer.mapped, 0, size) };
            allocations.push(Arc::new(CaptureAllocation(buffer)));
        }
        let storage: Vec<Arc<dyn CaptureStorage>> = allocations
            .iter()
            .map(|allocation| allocation.clone() as Arc<dyn CaptureStorage>)
            .collect();
        // SAFETY: these fresh private allocations are exposed to exactly this
        // pool. The encoder retains allocation owners only to bind a validated
        // borrowed frame; it never changes their contents independently.
        let pool = unsafe { CapturePool::new(storage, width, visible_height, stride) }
            .map_err(anyhow::Error::msg)?;
        self.capture_buffers = Some(CaptureBuffers {
            pool_id: pool.id(),
            width,
            visible_height,
            padded_height,
            stride,
            allocations,
        });
        Ok(pool)
    }

    fn native_input(&self, surface: NativeSurface<'_>) -> Result<&Buffer> {
        let inputs = self
            .capture_buffers
            .as_ref()
            .context("direct capture pool has not been allocated")?;
        let id = surface.allocation();
        ensure!(
            id.pool == inputs.pool_id,
            "native frame belongs to a different capture pool"
        );
        let buffer = &inputs
            .allocations
            .get(id.slot as usize)
            .context("invalid capture slot")?
            .0;
        ensure!(
            surface.width() == inputs.width
                && surface.height() == inputs.padded_height
                && surface.visible_height() == inputs.visible_height
                && surface.stride() == inputs.stride
                && surface.pixels().as_ptr() == buffer.mapped
                && surface.pixels().len() == buffer.size,
            "native frame does not match its registered capture allocation"
        );
        Ok(buffer)
    }

    fn capacity(&mut self, input: usize, strips: usize, cursor: usize) -> Result<()> {
        let sizes = [
            input,
            strips * 8,
            strips * OUTPUT_WORDS * 4,
            strips * 8,
            cursor.max(4),
        ];
        if self.buffers.len() == 5 && self.buffers.iter().zip(sizes).all(|(b, n)| b.size >= n) {
            return Ok(());
        }
        let mut buffers = Vec::new();
        // On an integrated GPU both processors use system RAM. Let the packer
        // write cached host-visible storage directly instead of copying every
        // 8192-byte slot into another allocation before reading its short body.
        let direct_output = self.info.device_type == "INTEGRATED_GPU";
        let mut previous = std::mem::take(&mut self.buffers).into_iter();
        for (i, size) in sizes.into_iter().enumerate() {
            if let Some(old) = previous.next().filter(|buffer| buffer.size >= size) {
                buffers.push(old);
            } else {
                buffers.push(Buffer::new(
                    self.ctx.clone(),
                    size,
                    matches!(i, 0 | 1 | 3 | 4) || (i == 2 && direct_output),
                )?);
                if i == 4 {
                    self.cursor_bitmap = None;
                }
            }
        }
        let readback = if direct_output {
            None
        } else {
            Some(
                match self
                    .readback
                    .take()
                    .filter(|buffer| buffer.size >= sizes[2])
                {
                    Some(buffer) => buffer,
                    None => Buffer::new(self.ctx.clone(), sizes[2], true)?,
                },
            )
        };
        unsafe {
            let infos: Vec<_> = buffers
                .iter()
                .map(|b| {
                    [vk::DescriptorBufferInfo::default()
                        .buffer(b.raw)
                        .range(b.size as u64)]
                })
                .collect();
            let writes: Vec<_> = infos
                .iter()
                .enumerate()
                .map(|(i, info)| {
                    vk::WriteDescriptorSet::default()
                        .dst_set(self.set)
                        .dst_binding(i as u32)
                        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                        .buffer_info(info)
                })
                .collect();
            self.ctx.device.update_descriptor_sets(&writes, &[]);
        }
        self.buffers = buffers;
        self.readback = readback;
        Ok(())
    }

    fn encode(
        &mut self,
        dock: kvino::DockProfile,
        width: usize,
        rgb: Option<&[u8]>,
        native: Option<NativeSurface<'_>>,
        coords: &[(usize, usize)],
    ) -> Result<Vec<Vec<u8>>> {
        if let Some(rgb) = rgb {
            crate::validate(dock, width, rgb, coords)?;
        } else {
            let surface = native.context("missing input surface")?;
            self.native_input(surface)?;
            ensure!(
                dock.video_on_ctrl_pipe() && dock.strip_dims() == (64, 16) && !dock.is_ten_bit(),
                "only the Ella 8-bit, 64x16 strip profile is supported"
            );
            for &(x, y) in coords {
                ensure!(
                    x.is_multiple_of(64)
                        && y.is_multiple_of(16)
                        && x < width
                        && y < surface.height(),
                    "strip coordinate ({x},{y}) is outside the native surface or unaligned"
                );
            }
        }
        if coords.is_empty() {
            return Ok(vec![]);
        }
        ensure!(coords.len() <= u32::MAX as usize / 48, "too many strips");
        let count = coords.len() as u32;
        let groups = count;
        let gx = groups.min(self.ctx.limits.max_compute_work_group_count[0]);
        let gy = groups.div_ceil(gx);
        ensure!(
            gy <= self.ctx.limits.max_compute_work_group_count[1]
                && count <= self.ctx.limits.max_compute_work_group_count[0],
            "dispatch exceeds device limits"
        );
        let cursor = native.and_then(|surface| surface.cursor());
        let bitmap = cursor.and_then(|snapshot| snapshot.bitmap());
        let cursor_bytes = bitmap
            .map(|bitmap| bitmap.words().len().checked_mul(4))
            .unwrap_or(Some(0))
            .context("cursor allocation size overflow")?;
        self.capacity(rgb.map_or(4, <[u8]>::len), coords.len(), cursor_bytes)?;
        let upload_start = Instant::now();
        let coordinate_words: Vec<u32> = coords
            .iter()
            .flat_map(|&(x, y)| [x as u32, y as u32])
            .collect();
        unsafe {
            let input = if let Some(rgb) = rgb {
                self.stage_times.pixel_upload_bytes +=
                    self.buffers[0].write_pixels(width, rgb, coords) as u64;
                &self.buffers[0]
            } else {
                self.native_input(native.unwrap())?
            };
            // The previous call has already completed its fence. In direct mode
            // this binds the EVDI allocation itself, with no desktop upload.
            let input_info = [vk::DescriptorBufferInfo::default()
                .buffer(input.raw)
                .range(input.size as u64)];
            self.ctx.device.update_descriptor_sets(
                &[vk::WriteDescriptorSet::default()
                    .dst_set(self.set)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&input_info)],
                &[],
            );
            if let Some(bitmap) = bitmap {
                if self
                    .cursor_bitmap
                    .as_ref()
                    .map(CursorBitmap::shape_revision)
                    != Some(bitmap.shape_revision())
                {
                    self.buffers[4].write(std::slice::from_raw_parts(
                        bitmap.words().as_ptr().cast(),
                        cursor_bytes,
                    ));
                    self.stage_times.cursor_upload_bytes += cursor_bytes as u64;
                    self.cursor_bitmap = Some(bitmap.clone());
                }
            }
            self.buffers[1].write(std::slice::from_raw_parts(
                coordinate_words.as_ptr().cast(),
                coordinate_words.len() * 4,
            ));
            self.stage_times.upload_ms += upload_start.elapsed().as_secs_f64() * 1000.0;
            let submit_start = Instant::now();
            let d = &self.ctx.device;
            let cmd = self.command;
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
            d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            if self.queries != vk::QueryPool::null() {
                d.cmd_reset_query_pool(cmd, self.queries, 0, 2);
            }
            let before = [vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::HOST_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE)];
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::HOST,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &before,
                &[],
                &[],
            );
            let position = cursor.map_or((0, 0), |cursor| cursor.position());
            let parameters = [
                width as u32,
                count,
                self.output_words as u32,
                gx,
                u32::from(native.is_some()),
                native.map_or(width * 3, |surface| surface.stride()) as u32,
                native.map_or_else(
                    || rgb.unwrap().len() / (width * 3),
                    |surface| surface.visible_height(),
                ) as u32,
                position.0 as u32,
                position.1 as u32,
                bitmap.map_or(0, |bitmap| bitmap.width()) as u32,
                bitmap.map_or(0, |bitmap| bitmap.height()) as u32,
                bitmap.map_or(0, |bitmap| bitmap.stride_words()) as u32,
            ];
            d.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::COMPUTE,
                self.pipeline_layout,
                0,
                &[self.set],
                &[],
            );
            d.cmd_push_constants(
                cmd,
                self.pipeline_layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                std::slice::from_raw_parts(parameters.as_ptr().cast(), 48),
            );
            d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.pipelines[0]);
            if self.queries != vk::QueryPool::null() {
                d.cmd_write_timestamp(cmd, vk::PipelineStageFlags::COMPUTE_SHADER, self.queries, 0);
            }
            d.cmd_dispatch(cmd, gx, gy, 1);
            if self.queries != vk::QueryPool::null() {
                d.cmd_write_timestamp(cmd, vk::PipelineStageFlags::COMPUTE_SHADER, self.queries, 1);
            }
            let to_copy = [vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ | vk::AccessFlags::HOST_READ)];
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COMPUTE_SHADER | vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::TRANSFER | vk::PipelineStageFlags::HOST,
                vk::DependencyFlags::empty(),
                &to_copy,
                &[],
                &[],
            );
            if let Some(readback) = &self.readback {
                d.cmd_copy_buffer(
                    cmd,
                    self.buffers[2].raw,
                    readback.raw,
                    &[vk::BufferCopy::default()
                        .size((coords.len() * self.output_words * 4) as u64)],
                );
                let after = [vk::MemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(vk::AccessFlags::HOST_READ)];
                d.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::HOST,
                    vk::DependencyFlags::empty(),
                    &after,
                    &[],
                    &[],
                );
            }
            d.end_command_buffer(cmd)?;
            d.reset_fences(&[self.fence])?;
            let commands = [cmd];
            let submitted = {
                let _queue = self
                    .ctx
                    .queue_lock
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                d.queue_submit(
                    self.ctx.queue,
                    &[vk::SubmitInfo::default().command_buffers(&commands)],
                    self.fence,
                )
            };
            if let Err(error) = submitted {
                let _ = self.ctx.wait_idle();
                bail!("submitting GPU encode: {error:?}");
            }
            // Report the fence error, then drain pending work before releasing its resources.
            if let Err(error) = d.wait_for_fences(&[self.fence], true, 30_000_000_000) {
                let _ = self.ctx.wait_idle();
                bail!("waiting for GPU completion: {error:?}");
            }
            self.stage_times.submit_wait_ms += submit_start.elapsed().as_secs_f64() * 1000.0;
            if self.queries != vk::QueryPool::null() {
                let mut ticks = [0u64; 2];
                d.get_query_pool_results(
                    self.queries,
                    0,
                    &mut ticks,
                    vk::QueryResultFlags::TYPE_64,
                )?;
                let mask = u64::MAX >> (64 - self.timestamp_bits);
                let ms = f64::from(self.ctx.limits.timestamp_period) * 1e-6;
                self.stage_times.fused_ms += (ticks[1].wrapping_sub(ticks[0]) & mask) as f64 * ms;
            }
            let read_start = Instant::now();
            self.dispatches += 1;
            self.encoded_strips += coords.len() as u64;
            self.stage_times.direct_dispatches += u64::from(native.is_some());
            ensure!(
                self.validation_errors() == 0,
                "Vulkan validation reported an error"
            );
            let metadata =
                std::slice::from_raw_parts(self.buffers[3].mapped.cast::<u32>(), coords.len() * 2);
            let mut result = Vec::with_capacity(coords.len());
            for i in 0..coords.len() {
                let length = metadata[i * 2] as usize;
                ensure!(
                    metadata[i * 2 + 1] == 0 && (16..=self.output_words * 4).contains(&length),
                    "GPU output overflow at strip {i}, limit {} bytes",
                    self.output_words * 4
                );
                result.push(
                    std::slice::from_raw_parts(
                        self.readback
                            .as_ref()
                            .unwrap_or(&self.buffers[2])
                            .mapped
                            .add(i * self.output_words * 4),
                        length,
                    )
                    .to_vec(),
                );
            }
            self.stage_times.readback_ms += read_start.elapsed().as_secs_f64() * 1000.0;
            Ok(result)
        }
    }
}
impl StripEncoder for GpuEncoder {
    type Error = anyhow::Error;
    fn compare_native_strips(
        &mut self,
        previous: NativeSurface<'_>,
        current: NativeSurface<'_>,
        strip_width: usize,
        strip_height: usize,
        changed: &mut [bool],
    ) -> Result<()> {
        ensure!(
            (strip_width, strip_height) == (64, 16),
            "GPU native comparison requires 64x16 strips"
        );
        self.compare_native(previous, current, changed)
    }
    fn encode_strips(
        &mut self,
        dock: kvino::DockProfile,
        width: usize,
        rgb: &[u8],
        coords: &[(usize, usize)],
    ) -> Result<Vec<Vec<u8>>> {
        self.encode(dock, width, Some(rgb), None, coords)
    }

    fn encode_native_strips(
        &mut self,
        dock: kvino::DockProfile,
        surface: NativeSurface<'_>,
        coords: &[(usize, usize)],
    ) -> Result<Vec<Vec<u8>>> {
        // This borrow includes the originating FrameLease's pixel slice. All
        // submissions (including a failed fence wait) are drained before it ends.
        self.encode(dock, surface.width(), None, Some(surface), coords)
    }
}
impl Drop for GpuEncoder {
    fn drop(&mut self) {
        unsafe {
            let d = &self.ctx.device;
            let _ = self.ctx.wait_idle();
            for &p in &self.pipelines {
                if p != vk::Pipeline::null() {
                    d.destroy_pipeline(p, None);
                }
            }
            if self.queries != vk::QueryPool::null() {
                d.destroy_query_pool(self.queries, None);
            }
            if self.fence != vk::Fence::null() {
                d.destroy_fence(self.fence, None);
            }
            if self.command_pool != vk::CommandPool::null() {
                d.destroy_command_pool(self.command_pool, None);
            }
            if self.descriptor_pool != vk::DescriptorPool::null() {
                d.destroy_descriptor_pool(self.descriptor_pool, None);
            }
            if self.pipeline_layout != vk::PipelineLayout::null() {
                d.destroy_pipeline_layout(self.pipeline_layout, None);
            }
            if self.layout != vk::DescriptorSetLayout::null() {
                d.destroy_descriptor_set_layout(self.layout, None);
            }
        }
    }
}
