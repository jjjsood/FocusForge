// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use focusforge_core::fuse::laplacian::score_plane;
use focusforge_core::fuse::metrics::{laplacian_variance, psnr};
use focusforge_core::fuse::{FusionConsumer, FusionGeometry, Plane, build};
use focusforge_core::io::{Rect, TiffStack};
use focusforge_core::params::{self, ParamValue, ParamValues, StackContext};
use focusforge_core::registry;
use focusforge_core::tile::{NoopPreprocess, TileScheduler, TileSchedulerOptions};
use focusforge_core::zmap::NO_SOURCE;
use serde::Deserialize;

const INPUT_DIR_ENV: &str = "FOCUSFORGE_ZMAP_GROUND_TRUTH_DIR";
const RADIUS_CORPUS_DIR_ENV: &str = "FOCUSFORGE_RADIUS_CORPUS_DIR";
const RANDOM_SEED: u64 = 0x9e37_79b9_7f4a_7c15;
const RANDOM_EXACT_TOLERANCE: f64 = 0.01;
const INITIAL_EXACT_COUNT: usize = 211_229;
const INITIAL_SAMPLE_COUNT: usize = 262_144;
const MEASURED_EXACT_FRACTION_FLOOR: f64 = 0.805;

#[derive(Debug, Deserialize)]
struct SceneMetadata {
    frames: usize,
    height: u32,
    noise: f64,
    noise_seed: u64,
    relief: String,
    scene: String,
    sigma_step: f64,
    texture_seed: u64,
    width: u32,
}

#[derive(Debug)]
struct SceneIdentity {
    scene: String,
    frames: usize,
    sigma_step: f64,
    noise: f64,
    noise_seed: u64,
    texture_seed: u64,
    relief: String,
}

impl std::fmt::Display for SceneIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "scene={}, frames={}, sigma_step={}, noise={}, noise_seed={}, texture_seed={}, relief={}",
            self.scene,
            self.frames,
            self.sigma_step,
            self.noise,
            self.noise_seed,
            self.texture_seed,
            self.relief
        )
    }
}

#[derive(Debug)]
struct Image {
    samples: Vec<u16>,
    width: u32,
    height: u32,
    samples_per_pixel: u32,
}

#[derive(Debug)]
struct Scene {
    source_dir: PathBuf,
    identity: SceneIdentity,
    width: u32,
    height: u32,
    samples_per_pixel: u32,
    frames: Vec<Vec<u16>>,
    reference: Vec<u16>,
    truth: Vec<u16>,
}

#[derive(Debug)]
struct SceneShape<'a> {
    width: u32,
    height: u32,
    samples_per_pixel: u32,
    frame_count: usize,
    frame_lengths: &'a [usize],
    reference_width: u32,
    reference_height: u32,
    reference_samples_per_pixel: u32,
    reference_length: usize,
    truth_width: u32,
    truth_height: u32,
    truth_samples_per_pixel: u32,
    truth_length: usize,
    truth: &'a [u16],
}

#[derive(Debug)]
struct FusionResult {
    composite: Vec<u16>,
    zmap: Vec<u16>,
}

/// The selected scene, from `FOCUSFORGE_ZMAP_GROUND_TRUTH_DIR`.
///
/// Unset: prints one `skip:` line naming the variable and returns `None`. Set
/// but the scene fails to load: panics, naming the problem -- unlike unset,
/// that is a misconfiguration, not the normal no-scene-available case.
fn selected_scene() -> Option<&'static Scene> {
    if std::env::var_os(INPUT_DIR_ENV).is_none() {
        eprintln!("skip: {INPUT_DIR_ENV} is not set");
        return None;
    }
    static SCENE: OnceLock<Scene> = OnceLock::new();
    Some(SCENE.get_or_init(|| load_selected_scene().unwrap_or_else(|error| panic!("{error}"))))
}

fn load_selected_scene() -> Result<Scene, String> {
    let dir = std::env::var_os(INPUT_DIR_ENV).ok_or_else(|| {
        format!(
            "selected scene mantis: {INPUT_DIR_ENV} is required; generate the selected scene and pass its directory explicitly"
        )
    })?;
    let dir = PathBuf::from(dir);
    load_scene(&dir, true)
}

fn load_training_scene(corpus: &Path, name: &str) -> Result<Scene, String> {
    let scene = load_scene(&corpus.join(name), false)?;
    let (expected_scene, expected_sigma_step) = match name {
        "train-centre" => ("hard", 0.45),
        "train-sigma-070" => ("hard", 0.70),
        "train-scene-easy" => ("easy", 0.45),
        _ => {
            return Err(format!(
                "training scene {name} is not selected for the radius ladder"
            ));
        }
    };
    let identity = &scene.identity;
    if identity.scene != expected_scene
        || identity.sigma_step != expected_sigma_step
        || identity.noise != 0.0
        || identity.frames != 8
        || identity.noise_seed != 11
        || identity.texture_seed != 7
        || identity.relief != "standard"
    {
        return Err(format!(
            "training scene {name}: metadata does not match the selected input: {identity}"
        ));
    }
    Ok(scene)
}

fn load_scene(dir: &Path, require_pinned_identity: bool) -> Result<Scene, String> {
    let metadata_path = dir.join("truth.json");
    let metadata_text = std::fs::read_to_string(&metadata_path).map_err(|error| {
        format!(
            "selected scene mantis: truth.json at {} is unavailable: {error}",
            metadata_path.display()
        )
    })?;
    let metadata: SceneMetadata = serde_json::from_str(&metadata_text).map_err(|error| {
        format!(
            "selected scene mantis: truth.json at {} is invalid: {error}",
            metadata_path.display()
        )
    })?;
    let identity = if require_pinned_identity {
        pinned_identity(&metadata)?
    } else {
        scene_identity(&metadata)?
    };
    let stack = TiffStack::open(dir.join("frames"))
        .map_err(|error| format!("{identity}: frames are unavailable: {error}"))?;
    let (width, height) = stack.dimensions();
    let samples_per_pixel = stack.header(0).samples_per_pixel;
    if (metadata.width, metadata.height) != (width, height) {
        return Err(format!(
            "{identity}: frame dimensions {width}x{height} do not match truth.json dimensions {}x{}",
            metadata.width, metadata.height
        ));
    }
    let rect = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let frames = (0..stack.len())
        .map(|index| {
            stack
                .read_tile(index, rect)
                .map(|tile| tile.samples)
                .map_err(|error| format!("{identity}: frame {index} cannot be read: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let reference = read_image(&dir.join("reference.tif"), &identity, "reference")?;
    let truth = read_image(&dir.join("truth-zmap.tif"), &identity, "truth")?;
    let frame_lengths = frames.iter().map(Vec::len).collect::<Vec<_>>();
    validate_scene_shape(
        &identity,
        SceneShape {
            width,
            height,
            samples_per_pixel,
            frame_count: frames.len(),
            frame_lengths: &frame_lengths,
            reference_width: reference.width,
            reference_height: reference.height,
            reference_samples_per_pixel: reference.samples_per_pixel,
            reference_length: reference.samples.len(),
            truth_width: truth.width,
            truth_height: truth.height,
            truth_samples_per_pixel: truth.samples_per_pixel,
            truth_length: truth.samples.len(),
            truth: &truth.samples,
        },
    )?;
    Ok(Scene {
        source_dir: dir.to_path_buf(),
        identity,
        width,
        height,
        samples_per_pixel,
        frames,
        reference: reference.samples,
        truth: truth.samples,
    })
}

fn scene_identity(metadata: &SceneMetadata) -> Result<SceneIdentity, String> {
    if metadata.width == 0 || metadata.height == 0 {
        return Err(format!(
            "scene={}: truth.json geometry is empty (width={}, height={})",
            metadata.scene, metadata.width, metadata.height
        ));
    }
    if metadata.frames == 0 {
        return Err(format!(
            "scene={}: truth.json records zero frames",
            metadata.scene
        ));
    }
    Ok(SceneIdentity {
        scene: metadata.scene.clone(),
        frames: metadata.frames,
        sigma_step: metadata.sigma_step,
        noise: metadata.noise,
        noise_seed: metadata.noise_seed,
        texture_seed: metadata.texture_seed,
        relief: metadata.relief.clone(),
    })
}

fn pinned_identity(metadata: &SceneMetadata) -> Result<SceneIdentity, String> {
    let identity = scene_identity(metadata)?;
    let stale = identity.scene != "mantis"
        || identity.frames != 8
        || identity.sigma_step != 0.45
        || identity.noise != 250.0
        || identity.noise_seed != 11
        || identity.texture_seed != 7
        || identity.relief != "standard";
    if stale {
        return Err(format!(
            "selected scene {identity}: truth.json does not match the pinned mantis generation parameters"
        ));
    }
    Ok(identity)
}

fn read_image(path: &Path, identity: &SceneIdentity, field: &str) -> Result<Image, String> {
    let stack = TiffStack::open_files(&[path.to_path_buf()])
        .map_err(|error| format!("{identity}: {field} is unavailable: {error}"))?;
    let (width, height) = stack.dimensions();
    let samples_per_pixel = stack.header(0).samples_per_pixel;
    let samples = stack
        .read_tile(
            0,
            Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
        )
        .map_err(|error| format!("{identity}: {field} cannot be read: {error}"))?
        .samples;
    Ok(Image {
        samples,
        width,
        height,
        samples_per_pixel,
    })
}

fn validate_scene_shape(identity: &SceneIdentity, shape: SceneShape<'_>) -> Result<(), String> {
    if shape.width == 0 || shape.height == 0 {
        return Err(format!("{identity}: frame dimensions are empty"));
    }
    if shape.samples_per_pixel != 3 {
        return Err(format!(
            "{identity}: frame samples_per_pixel is {}, expected RGB 3",
            shape.samples_per_pixel
        ));
    }
    if shape.frame_count != identity.frames {
        return Err(format!(
            "{identity}: frame count is {}, truth.json records {}",
            shape.frame_count, identity.frames
        ));
    }
    let frame_samples =
        shape.width as usize * shape.height as usize * shape.samples_per_pixel as usize;
    for (index, length) in shape.frame_lengths.iter().copied().enumerate() {
        if length != frame_samples {
            return Err(format!(
                "{identity}: frame {index} has {length} samples, expected {frame_samples}"
            ));
        }
    }
    if (
        shape.reference_width,
        shape.reference_height,
        shape.reference_samples_per_pixel,
    ) != (shape.width, shape.height, shape.samples_per_pixel)
    {
        return Err(format!(
            "{identity}: reference geometry {}x{}x{} does not match frame geometry {}x{}x{}",
            shape.reference_width,
            shape.reference_height,
            shape.reference_samples_per_pixel,
            shape.width,
            shape.height,
            shape.samples_per_pixel
        ));
    }
    if shape.reference_length != frame_samples {
        return Err(format!(
            "{identity}: reference has {} samples, expected {frame_samples}",
            shape.reference_length
        ));
    }
    if (shape.truth_width, shape.truth_height) != (shape.width, shape.height) {
        return Err(format!(
            "{identity}: truth dimensions {}x{} do not match frame dimensions {}x{}",
            shape.truth_width, shape.truth_height, shape.width, shape.height
        ));
    }
    if shape.truth_samples_per_pixel != 1 {
        return Err(format!(
            "{identity}: truth channel count is {}, expected 1",
            shape.truth_samples_per_pixel
        ));
    }
    let pixel_count = shape.width as usize * shape.height as usize;
    if shape.truth_length == 0 {
        return Err(format!("{identity}: truth is empty"));
    }
    if shape.truth_length != pixel_count {
        return Err(format!(
            "{identity}: truth length is {}, expected {pixel_count}",
            shape.truth_length
        ));
    }
    validate_indices(identity, "truth", shape.truth, shape.frame_count)
}

fn validate_indices(
    identity: &SceneIdentity,
    field: &str,
    indices: &[u16],
    frame_count: usize,
) -> Result<(), String> {
    for (pixel, index) in indices.iter().copied().enumerate() {
        if index == NO_SOURCE {
            return Err(format!("{identity}: {field}[{pixel}] is NO_SOURCE"));
        }
        if usize::from(index) >= frame_count {
            return Err(format!(
                "{identity}: {field}[{pixel}] is {index}, outside frame count {frame_count}"
            ));
        }
    }
    Ok(())
}

fn index_metrics(
    identity: &SceneIdentity,
    selected: &[u16],
    truth: &[u16],
    frame_count: usize,
) -> Result<(f64, f64, usize), String> {
    if selected.len() != truth.len() {
        return Err(format!(
            "{identity}: selected index length is {}, truth length is {}",
            selected.len(),
            truth.len()
        ));
    }
    if truth.is_empty() {
        return Err(format!("{identity}: index comparison has empty truth"));
    }
    validate_indices(identity, "selected", selected, frame_count)?;
    validate_indices(identity, "truth", truth, frame_count)?;
    let exact_count = selected
        .iter()
        .zip(truth)
        .filter(|(selected, truth)| selected == truth)
        .count();
    let total_error: u64 = selected
        .iter()
        .zip(truth)
        .map(|(&selected, &truth)| u64::from(selected.abs_diff(truth)))
        .sum();
    Ok((
        exact_count as f64 / truth.len() as f64,
        total_error as f64 / truth.len() as f64,
        exact_count,
    ))
}

fn composite_for_indices(scene: &Scene, indices: &[u16]) -> Result<Vec<u16>, String> {
    if indices.len() != scene.truth.len() {
        return Err(format!(
            "{}: selected index length is {}, truth length is {}",
            scene.identity,
            indices.len(),
            scene.truth.len()
        ));
    }
    validate_indices(&scene.identity, "selected", indices, scene.frames.len())?;
    let spp = scene.samples_per_pixel as usize;
    let mut composite = vec![0; scene.reference.len()];
    for (pixel, index) in indices.iter().copied().enumerate() {
        let offset = pixel * spp;
        composite[offset..offset + spp]
            .copy_from_slice(&scene.frames[usize::from(index)][offset..offset + spp]);
    }
    Ok(composite)
}

fn seeded_random_indices(pixel_count: usize, frame_count: usize) -> Vec<u16> {
    let mut state = RANDOM_SEED;
    (0..pixel_count)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 32) as usize % frame_count) as u16
        })
        .collect()
}

fn best_single_frame(scene: &Scene) -> (usize, f64) {
    let mut best = (
        0,
        psnr(
            &scene.frames[0],
            &scene.reference,
            scene.width,
            scene.height,
            scene.samples_per_pixel,
        ),
    );
    for (index, frame) in scene.frames.iter().enumerate().skip(1) {
        let score = psnr(
            frame,
            &scene.reference,
            scene.width,
            scene.height,
            scene.samples_per_pixel,
        );
        if score > best.1 {
            best = (index, score);
        }
    }
    best
}

fn fusion_output_dir(scene_dir: &Path) -> PathBuf {
    scene_dir.join("fusion-output")
}

fn run_fusion(scene: &Scene, algorithm_id: &str, explicit: &ParamValues) -> FusionResult {
    let stack = TiffStack::open(scene.source_dir.join("frames"))
        .expect("a validated selected scene's frames reopen");
    let context = StackContext::new(
        scene.frames.len(),
        scene.width,
        scene.height,
        scene.samples_per_pixel,
    );
    let algorithm = registry::find_stack(algorithm_id)
        .unwrap_or_else(|error| panic!("{algorithm_id} is registered: {error}"));
    let resolved = params::resolve(algorithm.params(), explicit, &context)
        .unwrap_or_else(|error| panic!("{algorithm_id} parameters resolve: {error}"));
    let values = params::resolved_values(&resolved);
    let scratch = fusion_output_dir(&scene.source_dir);
    std::fs::create_dir_all(&scratch).expect("fusion output directory is creatable");
    let composite_path = scratch.join(format!("{algorithm_id}.tif"));
    let zmap_path = scratch.join(format!("{algorithm_id}-zmap.tif"));
    let mut options = TileSchedulerOptions::explicit_budget(scratch.join("scheduler"), 1 << 30);
    options.apply_support(algorithm.tile_support(&values));
    options.tile_size = 256;
    let scheduler = TileScheduler::new(&stack, context, scene.samples_per_pixel, &options)
        .expect("scheduler builds");
    let consumer = FusionConsumer::create(
        algorithm,
        values,
        &composite_path,
        Some(&zmap_path),
        FusionGeometry {
            width: scene.width,
            height: scene.height,
            samples_per_pixel: scene.samples_per_pixel,
            frame_count: scene.frames.len() as u32,
            crop: None,
            description: None,
        },
    )
    .expect("fusion consumer creates");
    scheduler
        .run(&NoopPreprocess, &consumer)
        .expect("depth-map fusion completes");
    consumer.finish().expect("fusion output completes");
    let rect = Rect {
        x: 0,
        y: 0,
        width: scene.width,
        height: scene.height,
    };
    let composite = TiffStack::open_files(&[composite_path])
        .expect("composite reopens")
        .read_tile(0, rect)
        .expect("composite reads")
        .samples;
    let zmap = TiffStack::open_files(&[zmap_path])
        .expect("selection map reopens")
        .read_tile(0, rect)
        .expect("selection map reads")
        .samples;
    FusionResult { composite, zmap }
}

fn fusion_result(scene: &Scene) -> &'static FusionResult {
    static RESULT: OnceLock<FusionResult> = OnceLock::new();
    RESULT.get_or_init(|| {
        let mut explicit = ParamValues::new();
        explicit.insert("score-radius".to_string(), ParamValue::Int(4));
        explicit.insert("smoothing-radius".to_string(), ParamValue::Int(8));
        run_fusion(scene, "depth-map", &explicit)
    })
}

fn weighted_average_indices(scene: &Scene, radius: u32) -> Vec<u16> {
    let pixel_count = scene.width as usize * scene.height as usize;
    let mut best = vec![f32::NEG_INFINITY; pixel_count];
    let mut winner = vec![0u16; pixel_count];
    for (frame, samples) in scene.frames.iter().enumerate() {
        let detail = build(
            samples,
            scene.width,
            scene.height,
            scene.samples_per_pixel,
            1,
        )
        .expect("validated scene builds a detail pyramid");
        let scores = score_plane(&detail.levels()[0], radius);
        for pixel in 0..pixel_count {
            if scores[pixel] > best[pixel] {
                best[pixel] = scores[pixel];
                winner[pixel] = frame as u16;
            }
        }
    }
    winner
}

fn depth_map_indices(scene: &Scene, score_radius: u32, smoothing_radius: u32) -> Vec<u16> {
    let pixel_count = scene.width as usize * scene.height as usize;
    let mut best = vec![f32::NEG_INFINITY; pixel_count];
    let mut winner = vec![0u16; pixel_count];
    for (frame, samples) in scene.frames.iter().enumerate() {
        let detail = build(
            samples,
            scene.width,
            scene.height,
            scene.samples_per_pixel,
            1,
        )
        .expect("validated scene builds a detail pyramid");
        let raw = score_plane(&detail.levels()[0], score_radius);
        let mut plane = Plane::from_u16(&vec![0; pixel_count], scene.width, scene.height, 1)
            .expect("one score per pixel creates a plane");
        plane.data_mut().copy_from_slice(&raw);
        let scores = score_plane(&plane, smoothing_radius);
        for pixel in 0..pixel_count {
            if scores[pixel] > best[pixel] {
                best[pixel] = scores[pixel];
                winner[pixel] = frame as u16;
            }
        }
    }
    winner
}

fn print_radius_measurement(
    scene: &Scene,
    parameter: &str,
    radius: u32,
    selected: &[u16],
) -> (f64, f64, usize) {
    let (exact_fraction, mean_absolute_error, exact_count) =
        index_metrics(&scene.identity, selected, &scene.truth, scene.frames.len())
            .expect("experimental selection has valid indices");
    eprintln!(
        "{identity}; parameter={parameter}; radius={radius}; exact={exact_count}/{samples} ({exact_fraction:.6}); mean_absolute_index_error={mean_absolute_error:.6}",
        identity = scene.identity,
        samples = scene.truth.len(),
    );
    (exact_fraction, mean_absolute_error, exact_count)
}

fn weighted_average_values(radius: i64) -> ParamValues {
    let mut values = ParamValues::new();
    values.insert("radius".to_string(), ParamValue::Int(radius));
    values.insert("weight-exponent".to_string(), ParamValue::Float(2.0));
    values
}

fn depth_map_values(score_radius: i64, smoothing_radius: i64) -> ParamValues {
    let mut values = ParamValues::new();
    values.insert("score-radius".to_string(), ParamValue::Int(score_radius));
    values.insert(
        "smoothing-radius".to_string(),
        ParamValue::Int(smoothing_radius),
    );
    values
}

fn assert_production_parity(
    scene: &Scene,
    algorithm: &str,
    values: &ParamValues,
    expected: Vec<u16>,
) {
    let actual = run_fusion(scene, algorithm, values);
    assert_eq!(
        actual.zmap, expected,
        "{}: {algorithm} experimental selection must equal scheduler fusion",
        scene.identity
    );
}

#[test]
#[ignore = "requires an explicitly generated training corpus"]
fn radius_ceiling() {
    let corpus = std::env::var_os(RADIUS_CORPUS_DIR_ENV).unwrap_or_else(|| {
        panic!(
            "{RADIUS_CORPUS_DIR_ENV} is required; generate the training corpus and pass its directory explicitly"
        )
    });
    let candidates = [0u32, 4, 8, 16, 24, 32, 48, 64];
    let scene_names = ["train-centre", "train-sigma-070", "train-scene-easy"];

    for name in scene_names {
        let scene = load_training_scene(Path::new(&corpus), name)
            .unwrap_or_else(|error| panic!("training scene {name} must validate: {error}"));

        for (radius, values) in &[
            (0i64, weighted_average_values(0)),
            (8, weighted_average_values(8)),
            (16, weighted_average_values(16)),
        ] {
            assert_production_parity(
                &scene,
                "weighted-average",
                values,
                weighted_average_indices(&scene, *radius as u32),
            );
        }
        for &(score_radius, smoothing_radius) in &[(0i64, 8i64), (4, 8), (16, 8), (4, 0), (4, 16)] {
            assert_production_parity(
                &scene,
                "depth-map",
                &depth_map_values(score_radius, smoothing_radius),
                depth_map_indices(&scene, score_radius as u32, smoothing_radius as u32),
            );
        }

        let (truth_exact, truth_error, _) = index_metrics(
            &scene.identity,
            &scene.truth,
            &scene.truth,
            scene.frames.len(),
        )
        .expect("truth indices validate");
        let random = seeded_random_indices(scene.truth.len(), scene.frames.len());
        let (random_exact, random_error, _) =
            index_metrics(&scene.identity, &random, &scene.truth, scene.frames.len())
                .expect("random indices validate");
        let (best_frame, best_psnr) = best_single_frame(&scene);
        let best_single = vec![best_frame as u16; scene.truth.len()];
        let (single_exact, single_error, _) = index_metrics(
            &scene.identity,
            &best_single,
            &scene.truth,
            scene.frames.len(),
        )
        .expect("best-frame indices validate");
        eprintln!(
            "{}: controls truth_exact={truth_exact:.6} truth_error={truth_error:.6}; random seed {RANDOM_SEED:#x} exact={random_exact:.6} error={random_error:.6}; best_single frame={best_frame} exact={single_exact:.6} error={single_error:.6} psnr={best_psnr:.6}",
            scene.identity
        );
        assert_eq!((truth_exact, truth_error), (1.0, 0.0));
        assert!(random_exact < single_exact && single_exact < truth_exact);

        let mut weighted_counts = Vec::new();
        let mut score_counts = Vec::new();
        let mut smoothing_counts = Vec::new();

        for radius in candidates {
            let weighted = weighted_average_indices(&scene, radius);
            let weighted_repeat = weighted_average_indices(&scene, radius);
            assert_eq!(
                weighted, weighted_repeat,
                "{}: weighted-average radius={radius} must be deterministic",
                scene.identity
            );
            weighted_counts.push((
                radius,
                print_radius_measurement(&scene, "weighted-average.radius", radius, &weighted).2,
            ));

            let score = depth_map_indices(&scene, radius, 8);
            let score_repeat = depth_map_indices(&scene, radius, 8);
            assert_eq!(
                score, score_repeat,
                "{}: depth-map score-radius={radius} must be deterministic",
                scene.identity
            );
            score_counts.push((
                radius,
                print_radius_measurement(&scene, "depth-map.score-radius", radius, &score).2,
            ));

            let smoothing = depth_map_indices(&scene, 4, radius);
            let smoothing_repeat = depth_map_indices(&scene, 4, radius);
            assert_eq!(
                smoothing, smoothing_repeat,
                "{}: depth-map smoothing-radius={radius} must be deterministic",
                scene.identity
            );
            smoothing_counts.push((
                radius,
                print_radius_measurement(&scene, "depth-map.smoothing-radius", radius, &smoothing)
                    .2,
            ));
        }
        for (parameter, counts) in [
            ("weighted-average.radius", weighted_counts),
            ("depth-map.score-radius", score_counts),
            ("depth-map.smoothing-radius", smoothing_counts),
        ] {
            let at_16 = counts
                .iter()
                .find(|(radius, _)| *radius == 16)
                .expect("the ladder contains radius 16")
                .1;
            let at_24 = counts
                .iter()
                .find(|(radius, _)| *radius == 24)
                .expect("the ladder contains radius 24")
                .1;
            assert!(
                at_24 < at_16,
                "{}: {parameter} exact count must decline above 16: r16={at_16}, r24={at_24}",
                scene.identity
            );
        }
    }
}

#[test]
fn index_error_matches_hand_computed_cases() {
    let identity = SceneIdentity {
        scene: "hand-computed".to_string(),
        frames: 4,
        sigma_step: 0.0,
        noise: 0.0,
        noise_seed: 0,
        texture_seed: 0,
        relief: "none".to_string(),
    };
    assert_eq!(
        index_metrics(&identity, &[0, 1, 2], &[0, 1, 2], 4).unwrap(),
        (1.0, 0.0, 3)
    );
    assert_eq!(
        index_metrics(&identity, &[0, 2, 2], &[0, 1, 2], 4).unwrap(),
        (2.0 / 3.0, 1.0 / 3.0, 2)
    );
    assert_eq!(
        index_metrics(&identity, &[3, 1, 0], &[0, 1, 2], 4).unwrap(),
        (1.0 / 3.0, 5.0 / 3.0, 1)
    );
}

#[test]
fn stale_scene_metadata_is_refused_by_name() {
    let metadata = SceneMetadata {
        frames: 8,
        height: 512,
        noise: 250.0,
        noise_seed: 11,
        relief: "standard".to_string(),
        scene: "hard".to_string(),
        sigma_step: 0.45,
        texture_seed: 7,
        width: 512,
    };
    let error = pinned_identity(&metadata).unwrap_err();
    assert!(error.contains("scene=hard"));
    assert!(error.contains("does not match the pinned mantis generation parameters"));
}

#[test]
fn best_psnr_tie_prefers_the_first_frame() {
    let frame = vec![1234; 12];
    let scene = Scene {
        source_dir: PathBuf::new(),
        identity: SceneIdentity {
            scene: "tied-control".to_string(),
            frames: 2,
            sigma_step: 0.0,
            noise: 0.0,
            noise_seed: 0,
            texture_seed: 0,
            relief: "none".to_string(),
        },
        width: 2,
        height: 2,
        samples_per_pixel: 3,
        frames: vec![frame.clone(), frame.clone()],
        reference: frame,
        truth: vec![0; 4],
    };
    assert_eq!(best_single_frame(&scene).0, 0);
}

#[test]
fn fusion_outputs_stay_with_the_explicit_scene_directory() {
    assert_eq!(
        fusion_output_dir(Path::new("/explicit-scene")),
        PathBuf::from("/explicit-scene/fusion-output")
    );
}

#[test]
fn truth_mismatched_with_stack_is_refused_by_name() {
    let identity = SceneIdentity {
        scene: "malformed".to_string(),
        frames: 3,
        sigma_step: 0.45,
        noise: 250.0,
        noise_seed: 11,
        texture_seed: 7,
        relief: "standard".to_string(),
    };
    let frame_lengths = [12, 12, 12];
    let truth = [0, 1, 2, 0];
    let shape = || SceneShape {
        width: 2,
        height: 2,
        samples_per_pixel: 3,
        frame_count: 3,
        frame_lengths: &frame_lengths,
        reference_width: 2,
        reference_height: 2,
        reference_samples_per_pixel: 3,
        reference_length: 12,
        truth_width: 2,
        truth_height: 2,
        truth_samples_per_pixel: 1,
        truth_length: 4,
        truth: &truth,
    };
    let mut wrong_count = shape();
    wrong_count.frame_count = 2;
    assert!(
        validate_scene_shape(&identity, wrong_count)
            .unwrap_err()
            .contains("frame count")
    );
    let mut wrong_dimensions = shape();
    wrong_dimensions.truth_width = 3;
    assert!(
        validate_scene_shape(&identity, wrong_dimensions)
            .unwrap_err()
            .contains("truth dimensions")
    );
    let mut wrong_length = shape();
    wrong_length.truth_length = 3;
    assert!(
        validate_scene_shape(&identity, wrong_length)
            .unwrap_err()
            .contains("truth length")
    );
    let mut wrong_channels = shape();
    wrong_channels.truth_samples_per_pixel = 3;
    assert!(
        validate_scene_shape(&identity, wrong_channels)
            .unwrap_err()
            .contains("truth channel count")
    );
    let empty_truth = [];
    let mut empty = shape();
    empty.truth = &empty_truth;
    empty.truth_length = 0;
    assert!(
        validate_scene_shape(&identity, empty)
            .unwrap_err()
            .contains("truth is empty")
    );
    let no_source = [NO_SOURCE, 1, 2, 0];
    let mut missing_source = shape();
    missing_source.truth = &no_source;
    assert!(
        validate_scene_shape(&identity, missing_source)
            .unwrap_err()
            .contains("NO_SOURCE")
    );
    let out_of_range = [3, 1, 2, 0];
    let mut invalid_index = shape();
    invalid_index.truth = &out_of_range;
    assert!(
        validate_scene_shape(&identity, invalid_index)
            .unwrap_err()
            .contains("outside frame count")
    );
}

#[test]
fn index_error_is_asserted_against_a_measured_floor() {
    let Some(scene) = selected_scene() else {
        return;
    };
    let result = fusion_result(scene);
    let (exact_fraction, mean_absolute_error, exact_count) = index_metrics(
        &scene.identity,
        &result.zmap,
        &scene.truth,
        scene.frames.len(),
    )
    .unwrap();
    let composite_psnr = psnr(
        &result.composite,
        &scene.reference,
        scene.width,
        scene.height,
        scene.samples_per_pixel,
    );
    eprintln!(
        "{identity}; algorithm=depth-map; score_radius=4; smoothing_radius=8; tile_size=256; exact={exact_count}/{samples} ({exact_fraction:.6}); mean_absolute_index_error={mean_absolute_error:.6}; composite_psnr={composite_psnr:.6}; initial_exact={INITIAL_EXACT_COUNT}/{INITIAL_SAMPLE_COUNT}; floor={MEASURED_EXACT_FRACTION_FLOOR:.6}",
        identity = scene.identity,
        samples = scene.truth.len()
    );
    assert!(
        exact_fraction >= MEASURED_EXACT_FRACTION_FLOOR,
        "{identity}: depth-map exact fraction {exact_fraction:.6} ({exact_count}/{samples}) is below measured floor {MEASURED_EXACT_FRACTION_FLOOR:.6}; mean absolute index error {mean_absolute_error:.6}; composite PSNR {composite_psnr:.6} dB",
        identity = scene.identity,
        samples = scene.truth.len()
    );
}

#[test]
fn ground_truth_and_random_controls_bracket_every_candidate() {
    let Some(scene) = selected_scene() else {
        return;
    };
    let (truth_exact, truth_error, _) = index_metrics(
        &scene.identity,
        &scene.truth,
        &scene.truth,
        scene.frames.len(),
    )
    .unwrap();
    assert_eq!((truth_exact, truth_error), (1.0, 0.0));
    let random = seeded_random_indices(scene.truth.len(), scene.frames.len());
    let (random_exact, _, _) =
        index_metrics(&scene.identity, &random, &scene.truth, scene.frames.len()).unwrap();
    let random_expected = 1.0 / scene.frames.len() as f64;
    assert!(
        (random_exact - random_expected).abs() <= RANDOM_EXACT_TOLERANCE,
        "{}: random seed {RANDOM_SEED:#x} exact fraction {random_exact:.6} is outside {RANDOM_EXACT_TOLERANCE:.6} of 1/frame_count {random_expected:.6}",
        scene.identity
    );
    let (best_frame, best_psnr) = best_single_frame(scene);
    let single = vec![best_frame as u16; scene.truth.len()];
    let (single_exact, _, _) =
        index_metrics(&scene.identity, &single, &scene.truth, scene.frames.len()).unwrap();
    assert!(
        random_exact < single_exact && single_exact < truth_exact,
        "{}: random exact {random_exact:.6}, best single frame {best_frame} exact {single_exact:.6} (PSNR {best_psnr:.6} dB), truth exact {truth_exact:.6}",
        scene.identity
    );
}

#[test]
fn random_zmap_outscores_truth_on_laplacian_but_is_inaccurate() {
    let Some(scene) = selected_scene() else {
        return;
    };
    let random = seeded_random_indices(scene.truth.len(), scene.frames.len());
    let truth_composite = composite_for_indices(scene, &scene.truth).unwrap();
    let random_composite = composite_for_indices(scene, &random).unwrap();
    let truth_variance = laplacian_variance(
        &truth_composite,
        scene.width,
        scene.height,
        scene.samples_per_pixel,
    );
    let random_variance = laplacian_variance(
        &random_composite,
        scene.width,
        scene.height,
        scene.samples_per_pixel,
    );
    let (random_exact, _, _) =
        index_metrics(&scene.identity, &random, &scene.truth, scene.frames.len()).unwrap();
    eprintln!(
        "{}: random seed {RANDOM_SEED:#x}; random_laplacian_variance={random_variance:.6}; truth_laplacian_variance={truth_variance:.6}; random_exact_fraction={random_exact:.6}",
        scene.identity
    );
    assert!(
        random_variance > truth_variance,
        "{}: random seed {RANDOM_SEED:#x} Laplacian variance {random_variance:.6} must exceed truth {truth_variance:.6}",
        scene.identity
    );
    assert!(
        random_exact < 0.5,
        "{}: random seed {RANDOM_SEED:#x} exact fraction {random_exact:.6} must remain poor while its Laplacian variance is higher",
        scene.identity
    );
}
