// SPDX-License-Identifier: GPL-3.0-or-later

//! The device side of the ECC solve's sampling and masked reductions.
//!
//! A session holds one level's moving window and reference template resident
//! on the device. Each iteration writes a 64-byte parameter block, submits one
//! encoder of four dispatches and reads back the 18 moments (72 bytes); no
//! image data moves after the session opens.

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use focusforge_core::algorithm::AlgorithmError;
use focusforge_core::align::Transform;
use focusforge_core::gpu::{ECC_MOMENTS, EccMomentSession, EccSessionRequest};
use focusforge_core::tile::Rect;
use focusforge_core::timing::{TimingCounter, measurement_scope, record_timing};

use crate::budget::{OwnedPermit, VramBudget, buffer_limit_shortfalls};
use crate::device::{GpuError, SharedDevice, block_on};
use crate::errors;
use crate::pipeline::{Pipeline, build_pipeline, storage_entry, uniform_entry, upload_params};
use crate::transfer::{create_storage_buffer, upload_f32};

const SOURCE: &str = include_str!("ecc.wgsl");

/// Invocations per workgroup, as `@workgroup_size` in the kernel.
const WORKGROUP: u32 = 256;
/// Pixels one invocation takes before the grid-stride loop wraps.
const PIXELS_PER_INVOCATION: u32 = 4;
/// The most workgroups a sampling dispatch uses, whatever the region size.
const MAX_GROUPS: u32 = 1_024;
/// `f32` slots in one workgroup's partial row (the moment count).
const PARTIAL_SLOTS: u64 = ECC_MOMENTS as u64;
const PARAMS_BYTES: usize = 64;
const MOMENT_BYTES: u64 = ECC_MOMENTS as u64 * 4;
/// The result and staging buffers round the moments up to this.
const RESULT_BYTES: u64 = 128;
const WAIT_TIMEOUT: Duration = Duration::from_secs(60);

struct EccPipelines {
    sample: Pipeline,
    template: Pipeline,
    reduce_sample: Pipeline,
    reduce_template: Pipeline,
    #[cfg(feature = "gpu-tests")]
    values: Pipeline,
}

impl EccPipelines {
    fn new(device: &wgpu::Device) -> Self {
        let build = |label: &str, entry: &str, bindings: &[wgpu::BindGroupLayoutEntry]| {
            build_pipeline(device, SOURCE, label, entry, bindings)
        };
        EccPipelines {
            sample: build(
                "ecc-sample",
                "sample_moments",
                &[
                    storage_entry(0, true),
                    storage_entry(1, true),
                    storage_entry(2, true),
                    storage_entry(3, false),
                    uniform_entry(5),
                ],
            ),
            template: build(
                "ecc-template",
                "template_moments",
                &[
                    storage_entry(1, true),
                    storage_entry(2, true),
                    storage_entry(3, false),
                    storage_entry(4, false),
                    uniform_entry(5),
                ],
            ),
            reduce_sample: build(
                "ecc-reduce-sample",
                "reduce_sample",
                &[
                    storage_entry(3, false),
                    storage_entry(4, false),
                    uniform_entry(5),
                ],
            ),
            reduce_template: build(
                "ecc-reduce-template",
                "reduce_template",
                &[
                    storage_entry(3, false),
                    storage_entry(4, false),
                    uniform_entry(5),
                ],
            ),
            #[cfg(feature = "gpu-tests")]
            values: build(
                "ecc-values",
                "sample_values",
                &[
                    storage_entry(0, true),
                    storage_entry(4, false),
                    uniform_entry(5),
                ],
            ),
        }
    }
}

/// A prepared template and its Jacobian, resident on the device. Sessions of
/// one prepared template share it through [`EccState`]'s weak handles, so a
/// template no session holds frees its device memory.
struct ResidentTemplate {
    template: wgpu::Buffer,
    jacobian: wgpu::Buffer,
}

/// The ECC half of a [`crate::harness::GpuBackend`]: its pipelines, built on
/// first use, and the resident templates its live sessions share.
#[derive(Default)]
pub(crate) struct EccState {
    pipelines: OnceLock<EccPipelines>,
    templates: Mutex<HashMap<u64, Weak<ResidentTemplate>>>,
}

impl EccState {
    fn pipelines(&self, device: &wgpu::Device) -> &EccPipelines {
        self.pipelines.get_or_init(|| EccPipelines::new(device))
    }

    /// The resident copy of `request`'s template, uploaded when no live
    /// session already holds it.
    fn resident(
        &self,
        device: &SharedDevice,
        request: &EccSessionRequest<'_>,
    ) -> Arc<ResidentTemplate> {
        let mut cache = self
            .templates
            .lock()
            .expect("a panic never holds the template cache lock");
        if let Some(found) = cache.get(&request.template_id).and_then(Weak::upgrade) {
            return found;
        }
        cache.retain(|_, held| held.strong_count() > 0);
        let jacobian: Vec<f32> = request.jacobian.iter().map(|&g| g as f32).collect();
        let resident = Arc::new(ResidentTemplate {
            template: upload_f32(device.device(), device.queue(), request.template),
            jacobian: upload_f32(device.device(), device.queue(), &jacobian),
        });
        cache.insert(request.template_id, Arc::downgrade(&resident));
        resident
    }
}

/// The shape of one level's solve as the kernel sees it.
#[derive(Clone, Copy)]
struct Geometry {
    region: Rect,
    origin: (u32, u32),
    window: (u32, u32),
    columns: u32,
    pixels: u32,
    groups: u32,
    shift: f32,
    moving_bytes: u64,
    template_bytes: u64,
    jacobian_bytes: u64,
}

impl Geometry {
    /// `Ok(None)` when a buffer of the level exceeds the device's limits: the
    /// level is solved on the CPU instead. `Err` when `request` contradicts
    /// itself.
    fn new(
        device: &SharedDevice,
        request: &EccSessionRequest<'_>,
    ) -> Result<Option<Self>, AlgorithmError> {
        let region = request.region;
        let (win_w, win_h) = request.moving_dims;
        let pixels = u64::from(region.width) * u64::from(region.height);
        let columns = request.columns;
        if !matches!(columns, 2 | 4)
            || request.template.len() as u64 != pixels
            || request.jacobian.len() as u64 != pixels * columns as u64
            || request.moving.len() as u64 != u64::from(win_w) * u64::from(win_h)
        {
            return Err(AlgorithmError::Failed {
                algorithm: "gpu".to_string(),
                detail: "the ECC session request's buffers do not match its region, window and \
                         Jacobian columns"
                    .to_string(),
            });
        }
        let Ok(pixel_count) = u32::try_from(pixels) else {
            return Ok(None);
        };
        if pixel_count == 0 {
            return Ok(None);
        }
        let moving_bytes = request.moving.len() as u64 * 4;
        let template_bytes = pixels * 4;
        let jacobian_bytes = pixels * columns as u64 * 4;
        let limits = device.limits();
        let largest = moving_bytes.max(template_bytes).max(jacobian_bytes);
        if !buffer_limit_shortfalls(
            largest,
            limits.max_storage_buffer_binding_size,
            limits.max_buffer_size,
        )
        .is_empty()
        {
            return Ok(None);
        }
        Ok(Some(Geometry {
            region,
            origin: request.moving_origin,
            window: (win_w, win_h),
            columns: columns as u32,
            pixels: pixel_count,
            groups: groups_for(pixel_count, limits.max_compute_workgroups_per_dimension),
            shift: request.shift as f32,
            moving_bytes,
            template_bytes,
            jacobian_bytes,
        }))
    }

    /// The device bytes a session of this level holds, a shared template
    /// included.
    fn session_bytes(&self) -> u64 {
        self.moving_bytes
            + self.template_bytes
            + self.jacobian_bytes
            + u64::from(self.groups) * PARTIAL_SLOTS * 4
            + 2 * RESULT_BYTES
            + PARAMS_BYTES as u64
    }

    /// The kernel's parameter block for `warp`.
    fn params(&self, warp: &Transform) -> Result<[u8; PARAMS_BYTES], AlgorithmError> {
        let Some([a, b, bx, c, d, by]) =
            window_map(&warp.m, (self.region.x, self.region.y), self.origin)
        else {
            return Err(AlgorithmError::Failed {
                algorithm: "gpu".to_string(),
                detail: "the ECC kernel maps affine transforms only".to_string(),
            });
        };
        let words: [u32; PARAMS_BYTES / 4] = [
            a.to_bits(),
            b.to_bits(),
            bx.to_bits(),
            self.shift.to_bits(),
            c.to_bits(),
            d.to_bits(),
            by.to_bits(),
            0,
            self.region.width,
            self.pixels,
            self.window.0,
            self.window.1,
            self.groups,
            self.columns,
            (self.pixels as f32).to_bits(),
            0,
        ];
        let mut bytes = [0u8; PARAMS_BYTES];
        for (chunk, word) in bytes.chunks_exact_mut(4).zip(words) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        Ok(bytes)
    }
}

/// The workgroup count of a sampling dispatch over `pixels` pixels: a fixed
/// grid-stride count, never past [`MAX_GROUPS`] or the device's
/// per-dimension limit.
fn groups_for(pixels: u32, max_per_dimension: u32) -> u32 {
    pixels
        .div_ceil(WORKGROUP * PIXELS_PER_INVOCATION)
        .clamp(1, MAX_GROUPS.min(max_per_dimension).max(1))
}

/// `[a, b, bx, c, d, by]` with `x_window = a*i + b*j + bx` and
/// `y_window = c*i + d*j + by` for region pixel `(i, j)`: the map `m` (its
/// homogeneous scale divided out, as `Transform::map_point` does) taken to
/// window coordinates, the offset formed in `f64`. `None` for a projective or
/// degenerate map.
fn window_map(m: &[[f64; 3]; 3], region: (u32, u32), origin: (u32, u32)) -> Option<[f32; 6]> {
    let w = m[2][2];
    if m[2][0] != 0.0 || m[2][1] != 0.0 || w.is_nan() || w <= 0.0 {
        return None;
    }
    let (rx, ry) = (f64::from(region.0), f64::from(region.1));
    let (ox, oy) = (f64::from(origin.0), f64::from(origin.1));
    let bx = (m[0][0] * rx + m[0][1] * ry + m[0][2]) / w - ox;
    let by = (m[1][0] * rx + m[1][1] * ry + m[1][2]) / w - oy;
    Some([
        (m[0][0] / w) as f32,
        (m[0][1] / w) as f32,
        bx as f32,
        (m[1][0] / w) as f32,
        (m[1][1] / w) as f32,
        by as f32,
    ])
}

/// An accelerator failure as the driver sees it: a lost device or an
/// exhausted one can be redone on the CPU, anything else fails the run.
/// A failure caused by a loss can reach a thread before wgpu runs the
/// device-lost callback, so the flag gets a short grace before the failure is
/// called a plain one.
fn algorithm_error(device: &SharedDevice, err: GpuError) -> AlgorithmError {
    const LOSS_GRACE_POLLS: u32 = 10;
    let lost = (0..LOSS_GRACE_POLLS).any(|attempt| {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(10));
        }
        device.lost()
    });
    if matches!(err, GpuError::DeviceOutOfMemory { .. }) || lost {
        AlgorithmError::BackendLost {
            detail: err.to_string(),
        }
    } else {
        err.into()
    }
}

/// Runs `f` inside a validation scope and an out-of-memory scope, flattening
/// what either catches into a [`GpuError`].
fn scoped<T>(
    device: &wgpu::Device,
    f: impl FnOnce() -> Result<T, GpuError>,
) -> Result<T, GpuError> {
    errors::with_validation_scope(device, || {
        let scope = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let value = f();
        match block_on(scope.pop()) {
            // A value built after an allocation refusal is discarded: it
            // would hold error buffers.
            Some(err) => Err(GpuError::DeviceOutOfMemory {
                detail: err.to_string(),
            }),
            None => value,
        }
    })
    .and_then(std::convert::identity)
}

fn bind(
    device: &wgpu::Device,
    pipeline: &Pipeline,
    entries: &[(u32, &wgpu::Buffer)],
) -> wgpu::BindGroup {
    let entries: Vec<wgpu::BindGroupEntry<'_>> = entries
        .iter()
        .map(|&(binding, buffer)| wgpu::BindGroupEntry {
            binding,
            resource: buffer.as_entire_binding(),
        })
        .collect();
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("ecc-bind"),
        layout: &pipeline.bind_group_layout,
        entries: &entries,
    })
}

/// Encodes `stages` (pipeline, bind group, workgroup count) in one compute
/// pass and submits it, returning the submission index and recording the
/// submission.
fn submit_stages(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    stages: &[(&wgpu::ComputePipeline, &wgpu::BindGroup, u32)],
    copy_out: Option<(&wgpu::Buffer, &wgpu::Buffer)>,
) -> wgpu::SubmissionIndex {
    let mut encoder =
        device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("ecc") });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("ecc"),
            timestamp_writes: None,
        });
        for &(pipeline, bind_group, groups) in stages {
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, bind_group, &[]);
            pass.dispatch_workgroups(groups, 1, 1);
        }
    }
    if let Some((from, to)) = copy_out {
        encoder.copy_buffer_to_buffer(from, 0, to, 0, MOMENT_BYTES);
    }
    errors::record_dispatch("ecc-iteration");
    let index = queue.submit(Some(encoder.finish()));
    record_timing(TimingCounter::CpuSubmissions, 1);
    index
}

/// Opens a session for one level, or declines it when a buffer of the level
/// exceeds the device's limits.
pub(crate) fn open(
    device: &'static SharedDevice,
    budget: &Arc<VramBudget>,
    state: &EccState,
    request: &EccSessionRequest<'_>,
) -> Result<Option<Box<dyn EccMomentSession>>, AlgorithmError> {
    if device.lost() {
        return Err(AlgorithmError::BackendLost {
            detail: "the device was lost before the level opened".to_string(),
        });
    }
    let Some(geometry) = Geometry::new(device, request)? else {
        return Ok(None);
    };
    let permit = budget.acquire_owned(geometry.session_bytes());
    let wgpu_device = device.device();
    let session = scoped(wgpu_device, || {
        let pipelines = state.pipelines(wgpu_device);
        let template = state.resident(device, request);
        let moving = upload_f32(wgpu_device, device.queue(), request.moving);
        let partials = create_storage_buffer(
            wgpu_device,
            u64::from(geometry.groups) * PARTIAL_SLOTS * 4,
            "ecc-partials",
        );
        let result = create_storage_buffer(wgpu_device, RESULT_BYTES, "ecc-result");
        let params = upload_params(wgpu_device, device.queue(), &[0u8; PARAMS_BYTES]);
        record_timing(TimingCounter::BufferCreations, 1);
        let staging = wgpu_device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ecc-staging"),
            size: RESULT_BYTES,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let bind_groups = [
            bind(
                wgpu_device,
                &pipelines.sample,
                &[
                    (0, &moving),
                    (1, &template.template),
                    (2, &template.jacobian),
                    (3, &partials),
                    (5, &params),
                ],
            ),
            bind(
                wgpu_device,
                &pipelines.reduce_sample,
                &[(3, &partials), (4, &result), (5, &params)],
            ),
            bind(
                wgpu_device,
                &pipelines.template,
                &[
                    (1, &template.template),
                    (2, &template.jacobian),
                    (3, &partials),
                    (4, &result),
                    (5, &params),
                ],
            ),
            bind(
                wgpu_device,
                &pipelines.reduce_template,
                &[(3, &partials), (4, &result), (5, &params)],
            ),
        ];
        Ok(Session {
            device,
            geometry,
            pipelines: [
                pipelines.sample.compute.clone(),
                pipelines.reduce_sample.compute.clone(),
                pipelines.template.compute.clone(),
                pipelines.reduce_template.compute.clone(),
            ],
            bind_groups,
            params,
            result,
            staging,
            _template: template,
            _permit: permit,
        })
    })
    .map_err(|err| algorithm_error(device, err))?;
    Ok(Some(Box::new(session)))
}

/// One level's resident buffers and bind groups. The kernel sums in `f32` over
/// values minus the request's shift, which keeps the accumulation well
/// conditioned; the `f64` solver on the host is the correctness twin. The
/// template-side moments (`Σt`, `Σt²`, `Σg·t`, `Σg`) are produced only while
/// some sample is invalid: under full overlap the solver takes them from its
/// own precomputed statistics.
struct Session {
    device: &'static SharedDevice,
    geometry: Geometry,
    pipelines: [wgpu::ComputePipeline; 4],
    bind_groups: [wgpu::BindGroup; 4],
    params: wgpu::Buffer,
    result: wgpu::Buffer,
    staging: wgpu::Buffer,
    _template: Arc<ResidentTemplate>,
    /// Dropped last, after the session's buffers are released.
    _permit: OwnedPermit,
}

impl Session {
    fn iterate(&self, params: &[u8; PARAMS_BYTES]) -> Result<[f64; ECC_MOMENTS], GpuError> {
        let device = self.device.device();
        let queue = self.device.queue();
        queue.write_buffer(&self.params, 0, params);
        let groups = self.geometry.groups;
        let stages = [
            (&self.pipelines[0], &self.bind_groups[0], groups),
            (&self.pipelines[1], &self.bind_groups[1], 1),
            (&self.pipelines[2], &self.bind_groups[2], groups),
            (&self.pipelines[3], &self.bind_groups[3], 1),
        ];
        let index = submit_stages(device, queue, &stages, Some((&self.result, &self.staging)));

        let (tx, rx) = mpsc::channel();
        let slice = self.staging.slice(..MOMENT_BYTES);
        slice.map_async(wgpu::MapMode::Read, move |outcome| {
            let _ = tx.send(outcome);
        });
        {
            let _wait = measurement_scope("gpu_ecc_wait");
            device
                .poll(wgpu::PollType::Wait {
                    submission_index: Some(index),
                    timeout: Some(WAIT_TIMEOUT),
                })
                .map_err(|err| GpuError::Readback {
                    detail: format!("device.poll failed while waiting for the ECC moments: {err}"),
                })?;
        }
        rx.recv_timeout(WAIT_TIMEOUT)
            .map_err(|_| GpuError::Readback {
                detail: "the ECC moments' map callback did not fire after the submission completed"
                    .to_string(),
            })?
            .map_err(|err| GpuError::Readback {
                detail: format!("failed to map the ECC moments: {err}"),
            })?;

        // wgpu panics when the device is destroyed or lost between the map's
        // completion and this read; that is a failure of the iteration like
        // any other, reported as a loss once the flag is set.
        let moments = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut moments = [0.0f64; ECC_MOMENTS];
            {
                let view = slice.get_mapped_range();
                for (slot, word) in moments.iter_mut().zip(view.chunks_exact(4)) {
                    *slot = f64::from(f32::from_le_bytes(
                        word.try_into().expect("chunks_exact(4) yields 4 bytes"),
                    ));
                }
            }
            self.staging.unmap();
            moments
        }))
        .map_err(|_| GpuError::Readback {
            detail: "the ECC staging buffer was invalidated while it was read".to_string(),
        })?;
        record_timing(TimingCounter::UploadBytes, PARAMS_BYTES as u64);
        record_timing(TimingCounter::ReadbackBytes, MOMENT_BYTES);
        Ok(moments)
    }
}

impl EccMomentSession for Session {
    fn moments(&mut self, warp: &Transform) -> Result<[f64; ECC_MOMENTS], AlgorithmError> {
        let params = self.geometry.params(warp)?;
        let outcome = scoped(self.device.device(), || self.iterate(&params));

        #[cfg(feature = "gpu-tests")]
        {
            crate::harness::forced_device_loss(self.device);
            if outcome.is_ok() && self.device.lost() {
                return Err(AlgorithmError::BackendLost {
                    detail: "the device was lost during an ECC iteration".to_string(),
                });
            }
        }

        outcome.map_err(|err| algorithm_error(self.device, err))
    }
}

/// Every sample of `request`'s region through `warp` and whether its taps lay
/// inside the window, in region order: the sampler read back for the parity
/// tests.
#[cfg(feature = "gpu-tests")]
pub(crate) fn sample_values(
    device: &SharedDevice,
    state: &EccState,
    request: &EccSessionRequest<'_>,
    warp: &Transform,
) -> Result<Vec<Option<f32>>, AlgorithmError> {
    let Some(geometry) = Geometry::new(device, request)? else {
        return Err(AlgorithmError::Failed {
            algorithm: "gpu".to_string(),
            detail: "the ECC session request exceeds a device limit".to_string(),
        });
    };
    let params_bytes = geometry.params(warp)?;
    let wgpu_device = device.device();
    let pairs = scoped(wgpu_device, || {
        let pipelines = state.pipelines(wgpu_device);
        let moving = upload_f32(wgpu_device, device.queue(), request.moving);
        let out = create_storage_buffer(
            wgpu_device,
            u64::from(geometry.pixels) * 8,
            "ecc-sample-values",
        );
        let params = upload_params(wgpu_device, device.queue(), &params_bytes);
        let bind_group = bind(
            wgpu_device,
            &pipelines.values,
            &[(0, &moving), (4, &out), (5, &params)],
        );
        submit_stages(
            wgpu_device,
            device.queue(),
            &[(&pipelines.values.compute, &bind_group, geometry.groups)],
            None,
        );
        crate::transfer::download_f32(
            wgpu_device,
            device.queue(),
            &out,
            geometry.pixels as usize * 2,
        )
    })
    .map_err(|err| algorithm_error(device, err))?;
    Ok(pairs
        .chunks_exact(2)
        .map(|pair| (pair[1] > 0.5).then_some(pair[0]))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{groups_for, window_map};
    use focusforge_core::align::Transform;

    #[test]
    fn window_map_matches_map_point_when_the_homogeneous_scale_is_not_one() {
        // Solved transforms carry m[2][2] a few ulp off 1.0.
        let mut t = Transform::similarity(1.04, 0.0006, -90.2, -168.1);
        t.m[2][2] = 0.9999999999999963;
        let (region, origin) = ((300u32, 200u32), (250u32, 150u32));
        let [a, b, bx, c, d, by] = window_map(&t.m, region, origin).expect("affine");
        for (i, j) in [(0u32, 0u32), (1252, 835), (600, 400)] {
            let (sx, sy) = t
                .map_point(f64::from(region.0 + i), f64::from(region.1 + j))
                .expect("maps");
            let lx = f64::from(a) * f64::from(i) + f64::from(b) * f64::from(j) + f64::from(bx);
            let ly = f64::from(c) * f64::from(i) + f64::from(d) * f64::from(j) + f64::from(by);
            assert!(
                (lx - (sx - f64::from(origin.0))).abs() < 2e-3,
                "x at {i},{j}"
            );
            assert!(
                (ly - (sy - f64::from(origin.1))).abs() < 2e-3,
                "y at {i},{j}"
            );
        }
        t.m[2][0] = 1e-6;
        assert!(window_map(&t.m, region, origin).is_none());
    }

    #[test]
    fn the_dispatch_never_exceeds_the_workgroup_limit() {
        assert_eq!(groups_for(1, 65_535), 1);
        assert_eq!(groups_for(1_046_529, 65_535), 1_023);
        assert_eq!(groups_for(33_000_000, 65_535), 1_024);
        assert_eq!(groups_for(33_000_000, 512), 512);
    }
}
