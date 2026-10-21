// SPDX-License-Identifier: GPL-2.0-or-later
//! Reusable exact comparison pipeline. Unlike the encoder, it writes just one
//! word per strip and borrows two immutable native allocations simultaneously.

use super::*;

const SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/compare.spv"));
const PARAMETER_BYTES: u32 = 17 * 4;

pub(super) struct Comparison {
    ctx: Arc<DeviceContext>,
    layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pool: vk::DescriptorPool,
    set: vk::DescriptorSet,
    pipeline: vk::Pipeline,
    command_pool: vk::CommandPool,
    command: vk::CommandBuffer,
    fence: vk::Fence,
    queries: vk::QueryPool,
    timestamp_bits: u32,
    cursors: [Option<Buffer>; 2],
    shapes: [Option<u64>; 2],
    output: Option<Buffer>,
}

impl Comparison {
    pub(super) fn new(ctx: Arc<DeviceContext>, timestamp_bits: u32) -> Result<Self> {
        ensure!(
            ctx.limits.max_push_constants_size >= PARAMETER_BYTES,
            "GPU push constants cannot hold native comparison parameters"
        );
        let mut result = Self {
            ctx,
            layout: vk::DescriptorSetLayout::null(),
            pipeline_layout: vk::PipelineLayout::null(),
            pool: vk::DescriptorPool::null(),
            set: vk::DescriptorSet::null(),
            pipeline: vk::Pipeline::null(),
            command_pool: vk::CommandPool::null(),
            command: vk::CommandBuffer::null(),
            fence: vk::Fence::null(),
            queries: vk::QueryPool::null(),
            timestamp_bits,
            cursors: [None, None],
            shapes: [None, None],
            output: None,
        };
        unsafe {
            let d = &result.ctx.device;
            let bindings: Vec<_> = (0..5)
                .map(|binding| {
                    vk::DescriptorSetLayoutBinding::default()
                        .binding(binding)
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
                .size(PARAMETER_BYTES)];
            result.pipeline_layout = d.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&layouts)
                    .push_constant_ranges(&ranges),
                None,
            )?;
            let sizes = [vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(5)];
            result.pool = d.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(1)
                    .pool_sizes(&sizes),
                None,
            )?;
            result.set = d.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(result.pool)
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
            let words = ash::util::read_spv(&mut Cursor::new(SHADER))?;
            let module =
                d.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)?;
            let stage = vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::COMPUTE)
                .module(module)
                .name(c"main");
            let pipelines = d.create_compute_pipelines(
                vk::PipelineCache::null(),
                &[vk::ComputePipelineCreateInfo::default()
                    .stage(stage)
                    .layout(result.pipeline_layout)],
                None,
            );
            d.destroy_shader_module(module, None);
            match pipelines {
                Ok(pipelines) => result.pipeline = pipelines[0],
                Err((pipelines, error)) => {
                    for pipeline in pipelines {
                        d.destroy_pipeline(pipeline, None);
                    }
                    bail!("creating native comparison pipeline: {error:?}");
                }
            }
        }
        Ok(result)
    }

    pub(super) fn run(
        &mut self,
        previous: NativeSurface<'_>,
        current: NativeSurface<'_>,
        inputs: [vk::DescriptorBufferInfo; 2],
        flags: &mut [bool],
    ) -> Result<StageTimes> {
        let mut stages = StageTimes::default();
        let upload_start = Instant::now();
        let count = u32::try_from(flags.len()).context("too many comparison strips")?;
        ensure!(count > 0, "empty native comparison");
        let gx = count.min(self.ctx.limits.max_compute_work_group_count[0]);
        let gy = count.div_ceil(gx);
        ensure!(
            gy <= self.ctx.limits.max_compute_work_group_count[1],
            "native comparison dispatch exceeds device limits"
        );
        let surfaces = [previous, current];
        let snapshots = surfaces.map(|surface| surface.cursor());
        let bitmaps = snapshots.map(|snapshot| snapshot.and_then(|s| s.bitmap()));
        for (index, bitmap) in bitmaps.iter().enumerate() {
            let size = bitmap
                .map(|b| b.words().len().checked_mul(4))
                .unwrap_or(Some(0))
                .context("comparison cursor size overflow")?
                .max(4);
            if self.cursors[index]
                .as_ref()
                .is_none_or(|buffer| buffer.size < size)
            {
                self.cursors[index] = Some(Buffer::new(self.ctx.clone(), size, true)?);
                self.shapes[index] = None;
            }
            if let Some(bitmap) = bitmap {
                if self.shapes[index] != Some(bitmap.shape_revision()) {
                    unsafe {
                        self.cursors[index]
                            .as_ref()
                            .unwrap()
                            .write(std::slice::from_raw_parts(
                                bitmap.words().as_ptr().cast(),
                                bitmap.words().len() * 4,
                            ));
                    }
                    self.shapes[index] = Some(bitmap.shape_revision());
                    stages.compare_cursor_upload_bytes += (bitmap.words().len() * 4) as u64;
                }
            }
        }
        let output_bytes = flags
            .len()
            .checked_mul(4)
            .context("comparison output size overflow")?;
        if self
            .output
            .as_ref()
            .is_none_or(|buffer| buffer.size < output_bytes)
        {
            self.output = Some(Buffer::new(self.ctx.clone(), output_bytes, true)?);
        }
        let output = self.output.as_ref().unwrap();
        let mut parameters = [0u32; 17];
        parameters[..7].copy_from_slice(&[
            (current.width() / 64) as u32,
            count,
            gx,
            (previous.stride() / 4) as u32,
            previous.visible_height() as u32,
            (current.stride() / 4) as u32,
            current.visible_height() as u32,
        ]);
        for index in 0..2 {
            let position = snapshots[index].map_or((0, 0), |s| s.position());
            let bitmap = bitmaps[index];
            parameters[7 + index * 5..12 + index * 5].copy_from_slice(&[
                position.0 as u32,
                position.1 as u32,
                bitmap.map_or(0, |b| b.width()) as u32,
                bitmap.map_or(0, |b| b.height()) as u32,
                bitmap.map_or(0, |b| b.stride_words()) as u32,
            ]);
        }
        unsafe {
            let d = &self.ctx.device;
            let infos = [
                [inputs[0]],
                [inputs[1]],
                [vk::DescriptorBufferInfo::default()
                    .buffer(self.cursors[0].as_ref().unwrap().raw)
                    .range(self.cursors[0].as_ref().unwrap().size as u64)],
                [vk::DescriptorBufferInfo::default()
                    .buffer(self.cursors[1].as_ref().unwrap().raw)
                    .range(self.cursors[1].as_ref().unwrap().size as u64)],
                [vk::DescriptorBufferInfo::default()
                    .buffer(output.raw)
                    .range(output.size as u64)],
            ];
            let writes: Vec<_> = infos
                .iter()
                .enumerate()
                .map(|(index, info)| {
                    vk::WriteDescriptorSet::default()
                        .dst_set(self.set)
                        .dst_binding(index as u32)
                        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                        .buffer_info(info)
                })
                .collect();
            d.update_descriptor_sets(&writes, &[]);
            stages.compare_upload_ms = upload_start.elapsed().as_secs_f64() * 1000.0;
            let submit_start = Instant::now();
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
            // Capture writes are complete before the immutable lease is borrowed.
            // Cursor uploads use coherent mappings. Publish both host write sets.
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::HOST,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[vk::MemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::HOST_WRITE)
                    .dst_access_mask(vk::AccessFlags::SHADER_READ)],
                &[],
                &[],
            );
            d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.pipeline);
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
                std::slice::from_raw_parts(parameters.as_ptr().cast(), PARAMETER_BYTES as usize),
            );
            if self.queries != vk::QueryPool::null() {
                d.cmd_write_timestamp(cmd, vk::PipelineStageFlags::COMPUTE_SHADER, self.queries, 0);
            }
            d.cmd_dispatch(cmd, gx, gy, 1);
            if self.queries != vk::QueryPool::null() {
                d.cmd_write_timestamp(cmd, vk::PipelineStageFlags::COMPUTE_SHADER, self.queries, 1);
            }
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::PipelineStageFlags::HOST,
                vk::DependencyFlags::empty(),
                &[vk::MemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                    .dst_access_mask(vk::AccessFlags::HOST_READ)],
                &[],
                &[],
            );
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
                bail!("submitting native comparison: {error:?}");
            }
            // No borrowed native allocation can be reused until this finishes,
            // including the error path; draining precedes returning the error.
            if let Err(error) = d.wait_for_fences(&[self.fence], true, 30_000_000_000) {
                let _ = self.ctx.wait_idle();
                bail!("waiting for native comparison: {error:?}");
            }
            stages.compare_submit_wait_ms = submit_start.elapsed().as_secs_f64() * 1000.0;
            if self.queries != vk::QueryPool::null() {
                let mut ticks = [0u64; 2];
                d.get_query_pool_results(
                    self.queries,
                    0,
                    &mut ticks,
                    vk::QueryResultFlags::TYPE_64,
                )?;
                let mask = u64::MAX >> (64 - self.timestamp_bits);
                stages.compare_gpu_ms = (ticks[1].wrapping_sub(ticks[0]) & mask) as f64
                    * f64::from(self.ctx.limits.timestamp_period)
                    * 1e-6;
            }
            let read_start = Instant::now();
            let words = std::slice::from_raw_parts(output.mapped.cast::<u32>(), flags.len());
            for (flag, &word) in flags.iter_mut().zip(words) {
                ensure!(word <= 1, "invalid native comparison flag: {word}");
                *flag = word != 0;
            }
            stages.compare_readback_ms = read_start.elapsed().as_secs_f64() * 1000.0;
            stages.compare_dispatches = 1;
        }
        Ok(stages)
    }
}

impl Drop for Comparison {
    fn drop(&mut self) {
        let _ = self.ctx.wait_idle();
        unsafe {
            let d = &self.ctx.device;
            if self.pipeline != vk::Pipeline::null() {
                d.destroy_pipeline(self.pipeline, None);
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
            if self.pool != vk::DescriptorPool::null() {
                d.destroy_descriptor_pool(self.pool, None);
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

#[cfg(test)]
mod tests {
    use super::*;
    use vino_chimera::{
        capture_pool::{CaptureRect, FrameLease},
        surface::{CursorSnapshot, NativeAllocationId},
    };

    fn frame(pool: &Arc<CapturePool>, pixels: &[u8]) -> FrameLease {
        let mut writer = pool.acquire().unwrap();
        writer.pixels_mut().copy_from_slice(pixels);
        writer
            .complete_grab(&[CaptureRect {
                x1: 0,
                y1: 0,
                x2: pool.width() as i32,
                y2: pool.height() as i32,
            }])
            .unwrap()
            .unwrap()
    }

    fn surface<'a>(
        pool: &CapturePool,
        frame: &'a FrameLease,
        cursor: Option<&'a CursorSnapshot>,
    ) -> NativeSurface<'a> {
        NativeSurface::new(
            frame.pixels(),
            pool.width(),
            pool.height().div_ceil(16) * 16,
            pool.height(),
            pool.stride(),
            NativeAllocationId {
                pool: pool.id(),
                slot: frame.slot_id(),
            },
            cursor,
        )
        .unwrap()
    }

    // Explicitly opt in: this uses private Vulkan allocations only, never display or USB.
    #[test]
    #[ignore = "requires a real integrated Radeon GPU and Vulkan validation layer"]
    fn native_comparison_matches_scalar_pixels() {
        let mut gpu = GpuEncoder::new(None, true).unwrap();
        let pool = gpu
            .create_capture_pool_with_stride(128, 29, 32, 128 * 4 + 32)
            .unwrap();
        let stride = pool.stride();
        let mut source: Vec<u8> = (0..stride * 32)
            .map(|i| ((i * 193 + i / 31) & 255) as u8)
            .collect();
        let mut ignored = source.clone();
        for y in 0..32 {
            for x in 0..128 {
                ignored[y * stride + x * 4 + 3] ^= 255;
            }
            ignored[y * stride + 128 * 4..(y + 1) * stride].fill(211);
        }
        ignored[stride * 29..].fill(109);
        let first = frame(&pool, &source);
        let second = frame(&pool, &ignored);
        let empty = CursorSnapshot::default();
        let opaque = Arc::new(
            CursorBitmap::new(
                9,
                7,
                11,
                (0..77)
                    .map(|i| if i % 11 >= 9 { 0xa1b2c3d4 } else { 0xff1177cc })
                    .collect(),
            )
            .unwrap(),
        );
        let alpha = Arc::new(
            CursorBitmap::new(
                9,
                7,
                11,
                (0..77)
                    .map(|i| {
                        (([0, 1, 127, 128, 254, 255][i % 6] as u32) << 24)
                            | ((i * 9713) as u32 & 0xffffff)
                    })
                    .collect(),
            )
            .unwrap(),
        );
        let cursors = [
            empty,
            CursorSnapshot::new(Some(opaque.clone()), (61, 14), 1),
            CursorSnapshot::new(Some(opaque.clone()), (-4, -2), 2),
            CursorSnapshot::new(Some(alpha.clone()), (61, 14), 3),
            CursorSnapshot::new(Some(alpha.clone()), (125, 27), 4),
            CursorSnapshot::new(Some(alpha.clone()), (0, 29), 5),
            CursorSnapshot::new(Some(opaque.clone()), (i32::MIN, i32::MAX), 6),
            CursorSnapshot::new(Some(opaque.clone()), (i32::MAX, i32::MIN), 7),
        ];
        let mut flags = [true; 4];
        let mut checks = 0;
        let check = |gpu: &mut GpuEncoder,
                     a: NativeSurface<'_>,
                     b: NativeSurface<'_>,
                     flags: &mut [bool]| {
            gpu.compare_native(a, b, flags).unwrap();
            // Independent per-pixel logical RGB oracle; no region comparator or hashes.
            for (strip, flag) in flags.iter().enumerate() {
                let sx = (strip % 2) * 64;
                let sy = (strip / 2) * 16;
                let expected =
                    (sy..sy + 16).any(|y| (sx..sx + 64).any(|x| a.rgb_at(x, y) != b.rgb_at(x, y)));
                assert_eq!(
                    *flag,
                    expected,
                    "strip {strip}, {:?} -> {:?}",
                    a.cursor(),
                    b.cursor()
                );
            }
        };
        for a in &cursors {
            for b in &cursors {
                check(
                    &mut gpu,
                    surface(&pool, &first, Some(a)),
                    surface(&pool, &second, Some(b)),
                    &mut flags,
                );
                checks += 1;
            }
        }
        assert!(gpu.stage_times.compare_cursor_upload_bytes > 0);
        drop(second);
        // Every strip's final visible pixel, including the last partial strip row.
        for (x, y) in [(0, 0), (63, 15), (64, 16), (127, 28)] {
            let mut changed = ignored.clone();
            changed[y * stride + x * 4] ^= 1;
            let next = frame(&pool, &changed);
            check(
                &mut gpu,
                surface(&pool, &first, None),
                surface(&pool, &next, None),
                &mut flags,
            );
            checks += 1;
        }
        // Under a fully opaque cursor, background differences are logically hidden.
        source[14 * stride + 61 * 4] ^= 255;
        let hidden = frame(&pool, &source);
        check(
            &mut gpu,
            surface(&pool, &first, Some(&cursors[1])),
            surface(&pool, &hidden, Some(&cursors[1])),
            &mut flags,
        );
        assert_eq!(flags, [false; 4]);
        checks += 1;
        check(
            &mut gpu,
            surface(&pool, &first, Some(&cursors[1])),
            surface(&pool, &hidden, None),
            &mut flags,
        );
        checks += 1;
        // Identical-slot cursor changes are supported, though live code keeps its cheap CPU path.
        check(
            &mut gpu,
            surface(&pool, &hidden, Some(&cursors[1])),
            surface(&pool, &hidden, Some(&cursors[3])),
            &mut flags,
        );
        checks += 1;
        let before = gpu.stage_times.compare_dispatches;
        assert!(gpu
            .compare_native(
                surface(&pool, &first, None),
                surface(&pool, &hidden, None),
                &mut flags[..3]
            )
            .is_err());
        let fake = NativeSurface::new(
            first.pixels(),
            128,
            32,
            29,
            stride,
            NativeAllocationId {
                pool: pool.id() + 1,
                slot: first.slot_id(),
            },
            None,
        )
        .unwrap();
        assert!(gpu
            .compare_native(fake, surface(&pool, &hidden, None), &mut flags)
            .is_err());
        let copied = first.pixels().to_vec();
        let fake = NativeSurface::new(
            &copied,
            128,
            32,
            29,
            stride,
            NativeAllocationId {
                pool: pool.id(),
                slot: first.slot_id(),
            },
            None,
        )
        .unwrap();
        assert!(gpu
            .compare_native(fake, surface(&pool, &hidden, None), &mut flags)
            .is_err());
        assert_eq!(gpu.stage_times.compare_dispatches, before);
        assert_eq!(gpu.stage_times.compare_dispatches, checks);
        assert_eq!(gpu.stage_times.pixel_upload_bytes, 0);
        assert_eq!(gpu.validation_errors(), 0);
        println!("{checks} exact native comparisons passed with zero validation errors and desktop uploads");
    }
}
