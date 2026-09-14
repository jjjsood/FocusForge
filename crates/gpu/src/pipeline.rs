// SPDX-License-Identifier: GPL-3.0-or-later

//! The compute-pipeline plumbing every kernel family builds through: the
//! `Pipeline` type, bind-group-layout-entry helpers, the pipeline builder,
//! the dispatch-and-submit helpers and the uniform-buffer upload helper.

/// One compute pipeline plus the bind group layout it was built with.
pub(crate) struct Pipeline {
    compute: wgpu::ComputePipeline,
    bind_group_layout: wgpu::BindGroupLayout,
}

pub(crate) fn storage_entry(binding: u32, read_only: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

pub(crate) fn uniform_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

/// Builds one [`Pipeline`]: shader module, bind group layout, pipeline
/// layout, compute pipeline. `wgsl_src` is always an `include_str!` at the
/// call site, never a string literal. Every pipeline in this crate --
/// `crate::harness::GpuBackend`'s own harness kernel included -- is built
/// through this one function.
pub(crate) fn build_pipeline(
    device: &wgpu::Device,
    wgsl_src: &'static str,
    label: &str,
    entry_point: &str,
    bindings: &[wgpu::BindGroupLayoutEntry],
) -> Pipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(wgsl_src.into()),
    });
    let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(label),
        entries: bindings,
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(&bind_group_layout)],
        immediate_size: 0,
    });
    let compute = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(label),
        layout: Some(&pipeline_layout),
        module: &shader,
        entry_point: Some(entry_point),
        compilation_options: Default::default(),
        cache: None,
    });
    Pipeline {
        compute,
        bind_group_layout,
    }
}

/// Encodes one bind group over `entries`, one compute pass dispatching
/// `ceil(invocation_count / 64)` workgroups of `pipeline`, and submits it --
/// shared by every kernel family in this crate, `GpuBackend::self_check`
/// included, so each kernel wrapper is only its own buffer/params setup. A
/// zero invocation count dispatches nothing (a degenerate level-count-0
/// caller should not reach this, but this is cheaper than asserting it
/// never does).
pub(crate) fn run_pass(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipeline: &Pipeline,
    entries: &[wgpu::BindGroupEntry],
    invocation_count: u32,
    label: &str,
) {
    if invocation_count == 0 {
        return;
    }
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some(label),
        layout: &pipeline.bind_group_layout,
        entries,
    });
    let mut encoder =
        device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some(label) });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some(label),
            timestamp_writes: None,
        });
        pass.set_pipeline(&pipeline.compute);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.dispatch_workgroups(invocation_count.div_ceil(64), 1, 1);
    }
    queue.submit(Some(encoder.finish()));
}

/// A uniform buffer of exactly `bytes.len()` bytes: `create_buffer` +
/// `queue.write_buffer`, never `mapped_at_creation` -- every uniform buffer
/// in this crate, `GpuBackend::self_check`'s own params buffer included, is
/// built this way.
pub(crate) fn upload_params(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    bytes: &[u8],
) -> wgpu::Buffer {
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("focusforge-gpu-params"),
        size: bytes.len() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    queue.write_buffer(&buffer, 0, bytes);
    buffer
}
