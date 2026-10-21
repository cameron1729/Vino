// SPDX-License-Identifier: GPL-2.0-or-later
//! Experimental immutable DMA-buf snapshots copied by the GPU into the normal
//! capture pool. The external producer owns source lifetime/synchronization;
//! pool writer leases continue to own every destination write.

use super::{CaptureAllocation, DeviceContext, GpuEncoder};
use anyhow::{bail, ensure, Context, Result};
use ash::vk;
use serde::Serialize;
use std::{
    collections::VecDeque,
    mem,
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd},
    sync::Arc,
    time::{Duration, Instant},
};
use vino_chimera::{
    capture_pool::{CaptureRect, WriteLease},
    shared_capture::{SharedCaptureCopy, SharedCopyStats, SharedFramebuffer},
};

const XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
const MAX_IMPORTS: usize = 8;
const WAIT_NS: u64 = 500_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SourceKey {
    device: u64,
    inode: u64,
    length: u64,
}

struct ImportedBuffer {
    ctx: Arc<DeviceContext>,
    raw: vk::Buffer,
    memory: vk::DeviceMemory,
    key: SourceKey,
}

impl Drop for ImportedBuffer {
    fn drop(&mut self) {
        unsafe {
            if self.raw != vk::Buffer::null() {
                self.ctx.device.destroy_buffer(self.raw, None);
            }
            if self.memory != vk::DeviceMemory::null() {
                self.ctx.device.free_memory(self.memory, None);
            }
        }
    }
}

fn source_key(fd: BorrowedFd<'_>) -> Result<SourceKey> {
    let mut stat = mem::MaybeUninit::<libc::stat>::zeroed();
    // DMA-buf supports SEEK_END specifically to expose the complete allocation
    // size, including page padding. st_size is not reliable for every exporter.
    let (stat, length) = unsafe {
        ensure!(
            libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) == 0,
            "stat DMA-buf: {}",
            std::io::Error::last_os_error()
        );
        let length = libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_END);
        ensure!(
            length > 0,
            "DMA-buf does not expose a positive allocation size"
        );
        (stat.assume_init(), length as u64)
    };
    Ok(SourceKey {
        device: stat.st_dev,
        inode: stat.st_ino,
        length,
    })
}

fn validate_layout(source: &SharedFramebuffer<'_>, width: usize, height: usize) -> Result<u64> {
    ensure!(
        source.format == XRGB8888,
        "shared capture requires DRM_FORMAT_XRGB8888"
    );
    ensure!(
        source.modifier == 0,
        "shared capture requires DRM_FORMAT_MOD_LINEAR"
    );
    ensure!(
        source.width == width && source.height == height && width > 0 && height > 0,
        "shared framebuffer dimensions differ from the capture pool"
    );
    let row = width
        .checked_mul(4)
        .context("shared framebuffer row overflow")?;
    ensure!(
        source.stride >= row && source.stride.is_multiple_of(4) && source.offset.is_multiple_of(4),
        "shared framebuffer has an invalid or unaligned pitch/offset"
    );
    source
        .offset
        .checked_add(u64::try_from(
            source
                .stride
                .checked_mul(height - 1)
                .context("shared framebuffer size overflow")?,
        )?)
        .and_then(|n| n.checked_add(row as u64))
        .context("shared framebuffer extent overflow")
}

#[repr(C)]
#[derive(Default)]
struct SyncFileInfo {
    name: [u8; 32],
    status: i32,
    flags: u32,
    num_fences: u32,
    pad: u32,
    sync_fence_info: u64,
}

fn wait_producer(fd: BorrowedFd<'_>) -> Result<()> {
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(
            !remaining.is_zero(),
            "timed out waiting for shared framebuffer producer"
        );
        let result = unsafe { libc::poll(&mut poll, 1, remaining.as_millis().max(1) as i32) };
        if result < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        ensure!(
            result > 0,
            "waiting for shared framebuffer producer failed or timed out"
        );
        ensure!(
            poll.revents & (libc::POLLERR | libc::POLLNVAL) == 0
                && poll.revents & libc::POLLIN != 0,
            "shared framebuffer producer fence failed: poll events {:#x}",
            poll.revents
        );
        let mut info = SyncFileInfo::default();
        // Linux _IOWR('>', 4, struct sync_file_info), whose UAPI layout is fixed.
        const SYNC_IOC_FILE_INFO: libc::c_ulong =
            (3 << 30) | (56 << 16) | (b'>' as libc::c_ulong) << 8 | 4;
        ensure!(
            unsafe { libc::ioctl(fd.as_raw_fd(), SYNC_IOC_FILE_INFO, &mut info) } == 0,
            "shared framebuffer producer did not supply a valid sync_file"
        );
        ensure!(
            info.status > 0,
            "shared framebuffer producer reported fence status {}",
            info.status
        );
        return Ok(());
    }
}

impl ImportedBuffer {
    fn new(ctx: Arc<DeviceContext>, fd: BorrowedFd<'_>, key: SourceKey) -> Result<Self> {
        let mut result = Self {
            ctx,
            raw: vk::Buffer::null(),
            memory: vk::DeviceMemory::null(),
            key,
        };
        unsafe {
            let ctx = &result.ctx;
            let mut properties = vk::ExternalBufferProperties::default();
            ctx.instance
                .raw
                .get_physical_device_external_buffer_properties(
                    ctx.physical,
                    &vk::PhysicalDeviceExternalBufferInfo::default()
                        .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT),
                    &mut properties,
                );
            ensure!(
                properties
                    .external_memory_properties
                    .external_memory_features
                    .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE),
                "GPU cannot import DMA-buf transfer-source buffers"
            );
            let mut external = vk::ExternalMemoryBufferCreateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            result.raw = ctx.device.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(key.length)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .push_next(&mut external),
                None,
            )?;
            let mut dedicated = vk::MemoryDedicatedRequirements::default();
            let mut requirements = vk::MemoryRequirements2::default().push_next(&mut dedicated);
            ctx.device.get_buffer_memory_requirements2(
                &vk::BufferMemoryRequirementsInfo2::default().buffer(result.raw),
                &mut requirements,
            );
            let requirements = requirements.memory_requirements;
            ensure!(
                requirements.size <= key.length,
                "DMA-buf allocation is shorter than Vulkan buffer requirements"
            );
            let loader = ash::khr::external_memory_fd::Device::new(&ctx.instance.raw, &ctx.device);
            let mut fd_properties = vk::MemoryFdPropertiesKHR::default();
            loader
                .get_memory_fd_properties(
                    vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                    fd.as_raw_fd(),
                    &mut fd_properties,
                )
                .context("querying DMA-buf Vulkan memory types")?;
            let compatible = requirements.memory_type_bits & fd_properties.memory_type_bits;
            let memory_type = (0..ctx.memory.memory_type_count)
                .filter(|&i| {
                    compatible & (1 << i) != 0
                        && !ctx.memory.memory_types[i as usize]
                            .property_flags
                            .intersects(
                                vk::MemoryPropertyFlags::DEVICE_COHERENT_AMD
                                    | vk::MemoryPropertyFlags::PROTECTED
                                    | vk::MemoryPropertyFlags::LAZILY_ALLOCATED,
                            )
                })
                .max_by_key(|&i| {
                    u8::from(
                        ctx.memory.memory_types[i as usize]
                            .property_flags
                            .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL),
                    )
                })
                .context("DMA-buf has no compatible Vulkan memory type")?;
            let duplicate = fd
                .try_clone_to_owned()
                .context("duplicating DMA-buf for Vulkan")?;
            let mut import = vk::ImportMemoryFdInfoKHR::default()
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
                .fd(duplicate.as_raw_fd());
            let mut dedicated_info = vk::MemoryDedicatedAllocateInfo::default().buffer(result.raw);
            let mut allocation = vk::MemoryAllocateInfo::default()
                .allocation_size(key.length)
                .memory_type_index(memory_type)
                .push_next(&mut import);
            if dedicated.requires_dedicated_allocation != 0
                || properties
                    .external_memory_properties
                    .external_memory_features
                    .contains(vk::ExternalMemoryFeatureFlags::DEDICATED_ONLY)
            {
                allocation = allocation.push_next(&mut dedicated_info);
            }
            result.memory = ctx
                .device
                .allocate_memory(&allocation, None)
                .context("importing shared framebuffer DMA-buf")?;
            // Successful vkAllocateMemory transfers ownership of exactly this duplicate.
            let _ = duplicate.into_raw_fd();
            ctx.device
                .bind_buffer_memory(result.raw, result.memory, 0)?;
        }
        Ok(result)
    }
}

/// Owns an independent command pool/fence and may move onto the capture thread.
/// All queue submissions share DeviceContext's mutex; the encoder's command
/// buffers/descriptors remain private to the encoder thread.
pub struct SharedCaptureCopier {
    ctx: Arc<DeviceContext>,
    pool_id: u64,
    width: usize,
    height: usize,
    stride: usize,
    allocations: Vec<Arc<CaptureAllocation>>,
    command_pool: vk::CommandPool,
    command: vk::CommandBuffer,
    fence: vk::Fence,
    imports: VecDeque<ImportedBuffer>,
    regions: Vec<vk::BufferCopy>,
}

impl GpuEncoder {
    pub fn create_shared_capture_copier(&self) -> Result<SharedCaptureCopier> {
        ensure!(
            self.ctx.external_memory_fd,
            "GPU lacks DMA-buf import and foreign-queue extensions"
        );
        let inputs = self
            .capture_buffers
            .as_ref()
            .context("create the direct capture pool before its shared copier")?;
        let mut copier = SharedCaptureCopier {
            ctx: self.ctx.clone(),
            pool_id: inputs.pool_id,
            width: inputs.width,
            height: inputs.visible_height,
            stride: inputs.stride,
            allocations: inputs.allocations.clone(),
            command_pool: vk::CommandPool::null(),
            command: vk::CommandBuffer::null(),
            fence: vk::Fence::null(),
            imports: VecDeque::new(),
            regions: Vec::with_capacity(inputs.visible_height),
        };
        unsafe {
            copier.command_pool = copier.ctx.device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(copier.ctx.family)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )?;
            copier.command = copier.ctx.device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(copier.command_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )?[0];
            copier.fence = copier
                .ctx
                .device
                .create_fence(&vk::FenceCreateInfo::default(), None)?;
        }
        Ok(copier)
    }
}

impl SharedCaptureCopier {
    pub fn copy_frame(
        &mut self,
        source: SharedFramebuffer<'_>,
        writer: &mut WriteLease,
    ) -> Result<SharedCopyStats> {
        let started = Instant::now();
        ensure!(
            writer.pool_id() == self.pool_id,
            "shared copy writer belongs to another capture pool"
        );
        let required = validate_layout(&source, self.width, self.height)?;
        let target = self
            .allocations
            .get(writer.slot_id() as usize)
            .context("invalid capture writer slot")?;
        ensure!(
            writer.pixels_mut().as_mut_ptr() == target.0.mapped,
            "capture writer storage does not match its GPU buffer"
        );
        let key = source_key(source.fd)?;
        ensure!(
            source.size <= key.length && required <= source.size,
            "shared framebuffer pixels exceed the declared DMA-buf allocation"
        );
        if let Some(fence) = source.producer_fence {
            wait_producer(fence)?;
        }
        let index = self.imports.iter().position(|entry| entry.key == key);
        let imported = index.is_none();
        let index = if let Some(index) = index {
            index
        } else {
            let buffer = ImportedBuffer::new(self.ctx.clone(), source.fd, key)?;
            if self.imports.len() == MAX_IMPORTS {
                self.imports.pop_front();
            }
            self.imports.push_back(buffer);
            self.imports.len() - 1
        };
        let input = &self.imports[index];
        self.regions.clear();
        let row_bytes = self.width * 4;
        if source.stride == row_bytes && self.stride == row_bytes {
            self.regions.push(
                vk::BufferCopy::default()
                    .src_offset(source.offset)
                    .size((row_bytes * self.height) as u64),
            );
        } else {
            for y in 0..self.height {
                self.regions.push(
                    vk::BufferCopy::default()
                        .src_offset(source.offset + (source.stride * y) as u64)
                        .dst_offset((self.stride * y) as u64)
                        .size(row_bytes as u64),
                );
            }
        }
        unsafe {
            let d = &self.ctx.device;
            d.reset_command_buffer(self.command, vk::CommandBufferResetFlags::empty())?;
            d.begin_command_buffer(
                self.command,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            let acquire = [vk::BufferMemoryBarrier::default()
                .buffer(input.raw)
                .offset(0)
                .size(vk::WHOLE_SIZE)
                .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                .dst_queue_family_index(self.ctx.family)
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ)];
            d.cmd_pipeline_barrier(
                self.command,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &acquire,
                &[],
            );
            d.cmd_copy_buffer(self.command, input.raw, target.0.raw, &self.regions);
            let after = [vk::BufferMemoryBarrier::default()
                .buffer(target.0.raw)
                .offset(0)
                .size(vk::WHOLE_SIZE)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ | vk::AccessFlags::SHADER_READ)];
            d.cmd_pipeline_barrier(
                self.command,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::HOST | vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &after,
                &[],
            );
            let release = [vk::BufferMemoryBarrier::default()
                .buffer(input.raw)
                .offset(0)
                .size(vk::WHOLE_SIZE)
                .src_queue_family_index(self.ctx.family)
                .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                .src_access_mask(vk::AccessFlags::TRANSFER_READ)
                .dst_access_mask(vk::AccessFlags::empty())];
            d.cmd_pipeline_barrier(
                self.command,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &release,
                &[],
            );
            d.end_command_buffer(self.command)?;
            d.reset_fences(&[self.fence])?;
            let commands = [self.command];
            let submitted = {
                let _guard = self
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
                bail!("submitting shared framebuffer GPU copy: {error:?}");
            }
            if let Err(error) = d.wait_for_fences(&[self.fence], true, WAIT_NS) {
                // Never return a writer to the pool while the device can still write it.
                let _ = self.ctx.wait_idle();
                bail!("waiting for shared framebuffer GPU copy: {error:?}");
            }
        }
        ensure!(
            self.ctx
                .instance
                .errors
                .load(std::sync::atomic::Ordering::Relaxed)
                == 0,
            "Vulkan validation reported an error during shared capture"
        );
        Ok(SharedCopyStats {
            elapsed: started.elapsed(),
            bytes: (row_bytes * self.height) as u64,
            imports: u64::from(imported),
        })
    }
}

impl SharedCaptureCopy for SharedCaptureCopier {
    fn copy_frame(
        &mut self,
        source: SharedFramebuffer<'_>,
        writer: &mut WriteLease,
    ) -> std::result::Result<SharedCopyStats, String> {
        SharedCaptureCopier::copy_frame(self, source, writer).map_err(|error| format!("{error:#}"))
    }
}

impl Drop for SharedCaptureCopier {
    fn drop(&mut self) {
        // Every submission is synchronously drained before copy_frame returns.
        unsafe {
            if self.fence != vk::Fence::null() {
                self.ctx.device.destroy_fence(self.fence, None);
            }
            if self.command_pool != vk::CommandPool::null() {
                self.ctx
                    .device
                    .destroy_command_pool(self.command_pool, None);
            }
        }
    }
}

/// Private fixture allocation. It is never connected to EVDI, DRM or a display.
struct ExportedFixture {
    buffer: ImportedBuffer,
    mapped: *mut u8,
    fd: OwnedFd,
}
impl Drop for ExportedFixture {
    fn drop(&mut self) {
        unsafe {
            self.buffer.ctx.device.unmap_memory(self.buffer.memory);
        }
    }
}
impl ExportedFixture {
    fn new(ctx: Arc<DeviceContext>, bytes: usize) -> Result<Self> {
        let mut buffer = ImportedBuffer {
            ctx,
            raw: vk::Buffer::null(),
            memory: vk::DeviceMemory::null(),
            key: SourceKey {
                device: 0,
                inode: 0,
                length: 0,
            },
        };
        unsafe {
            let d = &buffer.ctx.device;
            let mut external = vk::ExternalMemoryBufferCreateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            buffer.raw = d.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(bytes as u64)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .push_next(&mut external),
                None,
            )?;
            let requirements = d.get_buffer_memory_requirements(buffer.raw);
            let memory_type = (0..buffer.ctx.memory.memory_type_count)
                .find(|&i| {
                    requirements.memory_type_bits & (1 << i) != 0
                        && buffer.ctx.memory.memory_types[i as usize]
                            .property_flags
                            .contains(
                                vk::MemoryPropertyFlags::HOST_VISIBLE
                                    | vk::MemoryPropertyFlags::HOST_COHERENT,
                            )
                        && !buffer.ctx.memory.memory_types[i as usize]
                            .property_flags
                            .intersects(
                                vk::MemoryPropertyFlags::DEVICE_COHERENT_AMD
                                    | vk::MemoryPropertyFlags::PROTECTED
                                    | vk::MemoryPropertyFlags::LAZILY_ALLOCATED,
                            )
                })
                .context("private DMA-buf fixture requires coherent host-visible memory")?;
            let mut export = vk::ExportMemoryAllocateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().buffer(buffer.raw);
            buffer.memory = d.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(memory_type)
                    .push_next(&mut export)
                    .push_next(&mut dedicated),
                None,
            )?;
            d.bind_buffer_memory(buffer.raw, buffer.memory, 0)?;
            let loader = ash::khr::external_memory_fd::Device::new(&buffer.ctx.instance.raw, d);
            let fd = OwnedFd::from_raw_fd(
                loader.get_memory_fd(
                    &vk::MemoryGetFdInfoKHR::default()
                        .memory(buffer.memory)
                        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT),
                )?,
            );
            let mapped = d
                .map_memory(
                    buffer.memory,
                    0,
                    vk::WHOLE_SIZE,
                    vk::MemoryMapFlags::empty(),
                )?
                .cast::<u8>();
            buffer.key = source_key(fd.as_fd())?;
            Ok(Self { buffer, mapped, fd })
        }
    }
}

#[derive(Serialize)]
pub struct SelfTestReport {
    pub device: super::DeviceInfo,
    pub validation_enabled: bool,
    pub cases: usize,
    pub copied_frames: usize,
    pub imported_allocations: u64,
    pub copied_bytes: u64,
    pub validation_errors: usize,
    pub full_hd_copy_cpu_ms: f64,
    pub full_hd_copy_wall_ms: f64,
    pub checked_native_encodes: usize,
    pub checked_native_comparisons: usize,
    pub checked_gpu_producer_copies: usize,
}

fn thread_cpu() -> Duration {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time);
    }
    Duration::new(time.tv_sec as u64, time.tv_nsec as u32)
}

/// Exercise DMA-buf export, import, cache reuse, row/offset handling and native
/// encoding against private RAM only. This cannot replace the live EVDI lease
/// test: that source belongs to the compositor and has its own reservation fence.
pub fn self_test(device: Option<usize>, validation: bool) -> Result<SelfTestReport> {
    use vino_chimera::{
        scanout::StripEncoder,
        surface::{NativeAllocationId, NativeSurface},
    };
    let mut encoder = GpuEncoder::new(device, validation)?;
    let mut report = SelfTestReport {
        device: encoder.info.clone(),
        validation_enabled: validation,
        cases: 0,
        copied_frames: 0,
        imported_allocations: 0,
        copied_bytes: 0,
        validation_errors: 0,
        full_hd_copy_cpu_ms: 0.0,
        full_hd_copy_wall_ms: 0.0,
        checked_native_encodes: 0,
        checked_native_comparisons: 0,
        checked_gpu_producer_copies: 0,
    };
    for (width, height, extra_pitch, offset) in
        [(128_usize, 33_usize, 32_usize, 256_u64), (1920, 1080, 0, 0)]
    {
        let source_pitch = width * 4 + extra_pitch;
        let destination_pitch = width * 4 + extra_pitch / 2;
        let padded_height = height.div_ceil(16) * 16;
        let pool = encoder.create_capture_pool_with_stride(
            width,
            height,
            padded_height,
            destination_pitch,
        )?;
        let mut copier = encoder.create_shared_capture_copier()?;
        let fixture =
            ExportedFixture::new(encoder.ctx.clone(), offset as usize + source_pitch * height)?;
        let source = unsafe {
            std::slice::from_raw_parts_mut(fixture.mapped, fixture.buffer.key.length as usize)
        };
        source.fill(0xd3);
        let mut cpu_total = Duration::ZERO;
        let mut wall_total = Duration::ZERO;
        let mut previous = None::<vino_chimera::capture_pool::FrameLease>;
        for iteration in 0..12_usize {
            for y in 0..height {
                for x in 0..width {
                    let pixel = offset as usize + y * source_pitch + x * 4;
                    source[pixel..pixel + 4].copy_from_slice(&[
                        (x * 13 + y * 3 + iteration) as u8,
                        (x + y * 17 + iteration * 3) as u8,
                        (x * 7 + y + iteration * 5) as u8,
                        (iteration * 29) as u8,
                    ]);
                }
            }
            let mut writer = pool.acquire().map_err(anyhow::Error::msg)?;
            let cpu = thread_cpu();
            let wall = Instant::now();
            let stats = copier.copy_frame(
                SharedFramebuffer {
                    fd: fixture.fd.as_fd(),
                    producer_fence: None,
                    width,
                    height,
                    stride: source_pitch,
                    offset,
                    modifier: 0,
                    format: XRGB8888,
                    size: fixture.buffer.key.length,
                },
                &mut writer,
            )?;
            if iteration >= 2 {
                cpu_total += thread_cpu() - cpu;
                wall_total += wall.elapsed();
            }
            ensure!(
                stats.imports == u64::from(iteration == 0),
                "DMA-buf import was not reused"
            );
            let frame = writer
                .complete_grab(&[CaptureRect {
                    x1: 0,
                    y1: 0,
                    x2: width as i32,
                    y2: height as i32,
                }])
                .map_err(anyhow::Error::msg)?
                .context("full GPU copy failed to publish")?;
            for y in 0..height {
                let src = offset as usize + source_pitch * y;
                let dst = destination_pitch * y;
                ensure!(
                    source[src..src + width * 4] == frame.pixels()[dst..dst + width * 4],
                    "GPU copy differs from its source at row {y}"
                );
                ensure!(
                    frame.pixels()[dst + width * 4..dst + destination_pitch]
                        .iter()
                        .all(|&v| v == 0),
                    "GPU copy overwrote destination row padding"
                );
            }
            ensure!(
                frame.pixels()[height * destination_pitch..]
                    .iter()
                    .all(|&v| v == 0),
                "GPU copy overwrote invisible destination rows"
            );
            fn view(
                frame: &vino_chimera::capture_pool::FrameLease,
                width: usize,
                padded_height: usize,
                height: usize,
                destination_pitch: usize,
            ) -> Result<NativeSurface<'_>> {
                NativeSurface::new(
                    frame.pixels(),
                    width,
                    padded_height,
                    height,
                    destination_pitch,
                    NativeAllocationId {
                        pool: frame.pool_id(),
                        slot: frame.slot_id(),
                    },
                    None,
                )
                .map_err(anyhow::Error::msg)
            }
            if let Some(previous) = &previous {
                let before = view(previous, width, padded_height, height, destination_pitch)?;
                let after = view(&frame, width, padded_height, height, destination_pitch)?;
                let mut cpu = vec![false; width / 64 * (padded_height / 16)];
                let mut gpu = cpu.clone();
                after.changed_strips(before, 64, 16, &mut cpu)?;
                encoder.compare_native(before, after, &mut gpu)?;
                ensure!(
                    cpu == gpu,
                    "native GPU comparison differs after DMA-buf capture"
                );
                report.checked_native_comparisons += 1;
            }
            if iteration == 0 {
                let surface = NativeSurface::new(
                    frame.pixels(),
                    width,
                    padded_height,
                    height,
                    destination_pitch,
                    NativeAllocationId {
                        pool: pool.id(),
                        slot: frame.slot_id(),
                    },
                    None,
                )
                .map_err(anyhow::Error::msg)?;
                let coords = vec![(0, 0), (width - 64, padded_height - 16)];
                let gpu = encoder.encode_native_strips(crate::ella(), surface, &coords)?;
                let mut rgb = Vec::with_capacity(width * padded_height * 3);
                for y in 0..padded_height {
                    for x in 0..width {
                        rgb.extend_from_slice(&surface.rgb_at(x, y));
                    }
                }
                let cpu = crate::CpuEncoder::new(1)?.encode_strips(
                    crate::ella(),
                    width,
                    &rgb,
                    &coords,
                )?;
                ensure!(
                    gpu == cpu,
                    "native encoder output differs after GPU DMA-buf capture"
                );
                report.checked_native_encodes += 1;
            }
            report.copied_frames += 1;
            report.imported_allocations += stats.imports;
            report.copied_bytes += stats.bytes;
            previous = Some(frame);
        }
        if width == 1920 {
            report.full_hd_copy_cpu_ms = cpu_total.as_secs_f64() * 1000.0 / 10.0;
            report.full_hd_copy_wall_ms = wall_total.as_secs_f64() * 1000.0 / 10.0;
        }
        report.cases += 1;
    }
    // Unlike the host-written fixtures above, this allocation is overwritten by
    // the GPU and handed across the foreign queue boundary before each import
    // read. Reusing the imported allocation must still observe every new fill.
    let (width, height, padded_height, pitch, offset) = (128, 33, 48, 544, 256);
    let pool = encoder.create_capture_pool(width, height, padded_height)?;
    let mut copier = encoder.create_shared_capture_copier()?;
    let fixture = ExportedFixture::new(encoder.ctx.clone(), offset + pitch * height)?;
    for (index, pattern) in [0x1122_3344_u32, 0xabcd_ef01, 0x0123_4567, 0xfedc_ba98]
        .into_iter()
        .enumerate()
    {
        fill_fixture_on_gpu(&mut copier, &fixture, pattern, index != 0)?;
        let mut writer = pool.acquire().map_err(anyhow::Error::msg)?;
        let stats = copier.copy_frame(
            SharedFramebuffer {
                fd: fixture.fd.as_fd(),
                producer_fence: None,
                width,
                height,
                stride: pitch,
                offset: offset as u64,
                modifier: 0,
                format: XRGB8888,
                size: fixture.buffer.key.length,
            },
            &mut writer,
        )?;
        ensure!(
            stats.imports == u64::from(index == 0),
            "GPU-written fixture import was not reused"
        );
        let frame = writer
            .complete_grab(&[CaptureRect {
                x1: 0,
                y1: 0,
                x2: width as i32,
                y2: height as i32,
            }])
            .map_err(anyhow::Error::msg)?
            .context("GPU-produced copy failed to publish")?;
        for pixel in frame.pixels()[..width * height * 4].chunks_exact(4) {
            ensure!(
                pixel == pattern.to_le_bytes(),
                "cached import observed stale GPU-producer pixels"
            );
        }
        ensure!(
            frame.pixels()[width * height * 4..].iter().all(|&v| v == 0),
            "GPU-produced copy overwrote codec padding"
        );
        report.checked_gpu_producer_copies += 1;
        report.copied_frames += 1;
        report.imported_allocations += stats.imports;
        report.copied_bytes += stats.bytes;
    }
    report.cases += 1;
    report.validation_errors = encoder.validation_errors();
    ensure!(
        report.validation_errors == 0,
        "Vulkan validation failed shared capture self-test"
    );
    Ok(report)
}

/// A GPU producer for the private correctness fixture. Reuse the copier's idle
/// command resources, keeping this test independent of extra device extensions.
/// Its completed fence establishes producer readiness before copy_frame; the
/// actual external memory visibility comes from the FOREIGN ownership barriers.
fn fill_fixture_on_gpu(
    copier: &mut SharedCaptureCopier,
    fixture: &ExportedFixture,
    value: u32,
    acquire: bool,
) -> Result<()> {
    unsafe {
        let d = &copier.ctx.device;
        let command = copier.command;
        d.reset_command_buffer(command, vk::CommandBufferResetFlags::empty())?;
        d.begin_command_buffer(
            command,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;
        if acquire {
            let barriers = [vk::BufferMemoryBarrier::default()
                .buffer(fixture.buffer.raw)
                .offset(0)
                .size(vk::WHOLE_SIZE)
                .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                .dst_queue_family_index(copier.ctx.family)
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)];
            d.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &barriers,
                &[],
            );
        }
        d.cmd_fill_buffer(command, fixture.buffer.raw, 0, vk::WHOLE_SIZE, value);
        let barriers = [vk::BufferMemoryBarrier::default()
            .buffer(fixture.buffer.raw)
            .offset(0)
            .size(vk::WHOLE_SIZE)
            .src_queue_family_index(copier.ctx.family)
            .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::empty())];
        d.cmd_pipeline_barrier(
            command,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::BOTTOM_OF_PIPE,
            vk::DependencyFlags::empty(),
            &[],
            &barriers,
            &[],
        );
        d.end_command_buffer(command)?;
        d.reset_fences(&[copier.fence])?;
        let commands = [command];
        let submitted = {
            let _guard = copier
                .ctx
                .queue_lock
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            d.queue_submit(
                copier.ctx.queue,
                &[vk::SubmitInfo::default().command_buffers(&commands)],
                copier.fence,
            )
        };
        if let Err(error) = submitted {
            let _ = copier.ctx.wait_idle();
            bail!("submitting private GPU producer: {error:?}");
        }
        if let Err(error) = d.wait_for_fences(&[copier.fence], true, WAIT_NS) {
            let _ = copier.ctx.wait_idle();
            bail!("waiting for private GPU producer: {error:?}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    #[test]
    fn source_extent_checks_layout_and_integer_overflow() {
        let file = std::fs::File::open("/dev/null").unwrap();
        let mut source = SharedFramebuffer {
            fd: file.as_fd(),
            producer_fence: None,
            width: 128,
            height: 33,
            stride: 544,
            offset: 256,
            modifier: 0,
            format: XRGB8888,
            size: 20_000,
        };
        assert_eq!(
            validate_layout(&source, 128, 33).unwrap(),
            256 + 32 * 544 + 512
        );
        source.modifier = 1;
        assert!(validate_layout(&source, 128, 33).is_err());
        source.modifier = 0;
        source.format = 0;
        assert!(validate_layout(&source, 128, 33).is_err());
        source.format = XRGB8888;
        source.stride = 508;
        assert!(validate_layout(&source, 128, 33).is_err());
        source.stride = 544;
        source.offset = u64::MAX - 3;
        assert!(validate_layout(&source, 128, 33).is_err());
    }

    #[test]
    fn rejects_non_sync_file_and_zero_size_source() {
        let file = std::fs::File::open("/dev/null").unwrap();
        assert!(wait_producer(file.as_fd()).is_err());
        assert!(source_key(file.as_fd()).is_err());
        assert_eq!(mem::size_of::<SyncFileInfo>(), 56);
    }

    #[test]
    #[ignore = "requires an integrated Radeon GPU; uses private DMA-bufs only"]
    fn private_dma_buf_copy_matches_cpu() {
        let report = self_test(None, true).unwrap();
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
    }
}
