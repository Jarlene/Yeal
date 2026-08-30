//! Ported unit tests for the GUI grounding engine.
//!
//! Sources: upstream `gui/tests/grounding.rs` and `gui/tests/coordinate_space.rs`
//! (PR understudy-ai/codex-cua#3). All tests are pure logic (prompt building,
//! JSON extraction, image-prep budget, coordinate-space transforms,
//! refinement-crop math, overlays, raw-response annotation) — no live GUI,
//! screen capture, or mouse/keyboard input, so none are `#[ignore]`.

use super::*;

use image::ImageFormat;

/// Upstream: `#[cfg(test)]` helper in `gui.rs` — maps a display-space point
/// into capture-relative logical coordinates for the coordinate-space tests.
fn local_point_within_state(state: &ObserveState, point: &HelperPoint) -> Option<HelperPoint> {
    let local_x = point.x - state.capture.origin_x;
    let local_y = point.y - state.capture.origin_y;
    if local_x >= 0.0
        && local_y >= 0.0
        && local_x < state.capture.width as f64
        && local_y < state.capture.height as f64
    {
        Some(HelperPoint {
            x: local_x,
            y: local_y,
        })
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// From upstream gui/tests/grounding.rs
// ---------------------------------------------------------------------------

#[test]
fn grounding_prompt_includes_retry_context_and_guide_hint() {
    let state = ObserveState {
        capture: CaptureArtifact {
            origin_x: 0.0,
            origin_y: 0.0,
            width: 800,
            height: 600,
            image_width: 1600,
            image_height: 1200,
            display_index: 1,
            capture_mode: CaptureMode::Window,
            window_title: Some("Quick Note".to_string()),
            window_count: Some(1),
            window_capture_strategy: Some("selected_window".to_string()),
            host_exclusion: HostCaptureExclusionState::default(),
        },
        app_name: Some("Notes".to_string()),
    };

    let prompt = build_gui_grounding_prompt(
        GuiTargetRequest {
            app: Some("Notes"),
            capture_mode: Some("window"),
            window_selection: None,
            target: "Save button",
            location_hint: Some("top right"),
            scope: Some("toolbar"),
            grounding_mode: Some("complex"),
            action: "click",
            related_target: None,
            related_scope: None,
            related_location_hint: None,
            related_point: None,
        },
        &state,
        "complex",
        &[
            "Round 1 validator rejected the candidate".to_string(),
            "Move away from the highlighted point".to_string(),
        ],
        true,
    );

    assert!(prompt.contains("Retry context:"));
    assert!(prompt.contains("Round 1 validator rejected the candidate"));
    assert!(prompt.contains("additional guide image"));
    assert!(prompt.contains("Match the target by visible meaning"));
    assert!(prompt.contains("Match subtle or weakly labeled controls"));
    assert!(prompt.contains("button, icon button, toolbar item"));
    assert!(prompt.contains("individual icon-bearing control"));
    assert!(prompt.contains("visible button label inside a dialog"));
}

#[test]
fn grounding_prompt_adds_action_specific_editable_surface_guidance() {
    let state = ObserveState {
        capture: CaptureArtifact {
            origin_x: 0.0,
            origin_y: 0.0,
            width: 800,
            height: 600,
            image_width: 1600,
            image_height: 1200,
            display_index: 1,
            capture_mode: CaptureMode::Window,
            window_title: Some("Quick Note".to_string()),
            window_count: Some(1),
            window_capture_strategy: Some("selected_window".to_string()),
            host_exclusion: HostCaptureExclusionState::default(),
        },
        app_name: Some("Notes".to_string()),
    };

    let prompt = build_gui_grounding_prompt(
        GuiTargetRequest {
            app: Some("Notes"),
            capture_mode: Some("window"),
            window_selection: None,
            target: "Search field",
            location_hint: Some("top right"),
            scope: Some("toolbar"),
            grounding_mode: Some("complex"),
            action: "type",
            related_target: None,
            related_scope: None,
            related_location_hint: None,
            related_point: None,
        },
        &state,
        "complex",
        &[],
        false,
    );

    assert!(prompt.contains("editable surface itself"));
    assert!(prompt.contains("icon-only affordance"));
}

#[test]
fn not_found_retry_notes_encourage_semantic_role_flexibility() {
    let notes = build_not_found_retry_notes(
        GuiTargetRequest {
            app: Some("Notes"),
            capture_mode: Some("window"),
            window_selection: None,
            target: "Search field",
            location_hint: Some("top right"),
            scope: Some("toolbar"),
            grounding_mode: Some("complex"),
            action: "click",
            related_target: None,
            related_scope: None,
            related_location_hint: None,
            related_point: None,
        },
        1,
    );

    assert!(
        notes
            .iter()
            .any(|note| note.contains("Broaden the search while keeping the same semantic goal"))
    );
    assert!(notes.iter().any(|note| note.contains("visible meaning")));
    assert!(notes.iter().any(|note| note.contains("toolbar items")));
}

#[test]
fn validation_prompt_requests_failure_kind_and_retry_hint() {
    let state = ObserveState {
        capture: CaptureArtifact {
            origin_x: 0.0,
            origin_y: 0.0,
            width: 800,
            height: 600,
            image_width: 1600,
            image_height: 1200,
            display_index: 1,
            capture_mode: CaptureMode::Window,
            window_title: Some("Quick Note".to_string()),
            window_count: Some(1),
            window_capture_strategy: Some("selected_window".to_string()),
            host_exclusion: HostCaptureExclusionState::default(),
        },
        app_name: Some("Notes".to_string()),
    };

    let prompt = build_gui_grounding_validation_prompt(
        GuiTargetRequest {
            app: Some("Notes"),
            capture_mode: Some("window"),
            window_selection: None,
            target: "Save button",
            location_hint: Some("top right"),
            scope: Some("toolbar"),
            grounding_mode: Some("complex"),
            action: "click",
            related_target: None,
            related_scope: None,
            related_location_hint: None,
            related_point: None,
        },
        &state,
        &HelperPoint { x: 320.0, y: 180.0 },
        Some(&GroundingBoundingBox {
            x1: 280.0,
            y1: 140.0,
            x2: 360.0,
            y2: 220.0,
        }),
        false,
    );

    assert!(prompt.contains("failure_kind"));
    assert!(prompt.contains("retry_hint"));
    assert!(prompt.contains("wrong_region"));
    assert!(prompt.contains("whitespace, padding, decoration"));
    assert!(prompt.contains("subtle, tightly packed, or low-contrast controls"));
    assert!(prompt.contains("strongest visible semantic match"));
}

#[test]
fn validation_prompt_mentions_zoomed_crop_when_requested() {
    let state = ObserveState {
        capture: CaptureArtifact {
            origin_x: 100.0,
            origin_y: 200.0,
            width: 360,
            height: 320,
            image_width: 1200,
            image_height: 1067,
            display_index: 1,
            capture_mode: CaptureMode::Window,
            window_title: Some("Quick Note".to_string()),
            window_count: Some(1),
            window_capture_strategy: Some("selected_window".to_string()),
            host_exclusion: HostCaptureExclusionState::default(),
        },
        app_name: Some("Notes".to_string()),
    };

    let prompt = build_gui_grounding_validation_prompt(
        GuiTargetRequest {
            app: Some("Notes"),
            capture_mode: Some("window"),
            window_selection: None,
            target: "Pin icon button",
            location_hint: Some("toolbar row"),
            scope: Some("toolbar"),
            grounding_mode: Some("complex"),
            action: "click",
            related_target: None,
            related_scope: None,
            related_location_hint: None,
            related_point: None,
        },
        &state,
        &HelperPoint { x: 640.0, y: 400.0 },
        None,
        true,
    );

    assert!(prompt.contains("zoomed crop"));
    assert!(prompt.contains("original request"));
}

#[test]
fn retry_guide_is_suppressed_for_wrong_region_and_scope_mismatch() {
    assert!(!should_generate_retry_guide(Some("wrong_region")));
    assert!(!should_generate_retry_guide(Some("scope_mismatch")));
    assert!(should_generate_retry_guide(Some("wrong_control")));
    assert!(should_generate_retry_guide(None));
}

#[test]
fn refinement_prompt_mentions_zoomed_crop_context() {
    let state = ObserveState {
        capture: CaptureArtifact {
            origin_x: 0.0,
            origin_y: 0.0,
            width: 800,
            height: 600,
            image_width: 1600,
            image_height: 1200,
            display_index: 1,
            capture_mode: CaptureMode::Window,
            window_title: Some("Quick Note".to_string()),
            window_count: Some(1),
            window_capture_strategy: Some("selected_window".to_string()),
            host_exclusion: HostCaptureExclusionState::default(),
        },
        app_name: Some("Notes".to_string()),
    };
    let crop = create_refinement_crop(
        &{
            let base = image::RgbaImage::from_pixel(1200, 900, image::Rgba([240, 240, 240, 255]));
            let mut encoded = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(base)
                .write_to(&mut encoded, ImageFormat::Png)
                .expect("base image should encode");
            encoded.into_inner()
        },
        &HelperPoint { x: 420.0, y: 360.0 },
        Some(&GroundingBoundingBox {
            x1: 400.0,
            y1: 340.0,
            x2: 440.0,
            y2: 380.0,
        }),
    )
    .expect("refinement crop should build")
    .expect("refinement crop should exist");

    let prompt = build_gui_grounding_refinement_prompt(
        GuiTargetRequest {
            app: Some("Notes"),
            capture_mode: Some("window"),
            window_selection: None,
            target: "Save button",
            location_hint: Some("top right"),
            scope: Some("toolbar"),
            grounding_mode: Some("complex"),
            action: "click",
            related_target: None,
            related_scope: None,
            related_location_hint: None,
            related_point: None,
        },
        &state,
        &crop,
        Some(&HelperPoint { x: 100.0, y: 120.0 }),
        Some(&GroundingBoundingBox {
            x1: 80.0,
            y1: 100.0,
            x2: 140.0,
            y2: 160.0,
        }),
    );

    assert!(prompt.contains("zoomed crop around a previous candidate"));
    assert!(prompt.contains("Previous crop-relative point"));
    assert!(prompt.contains("Refine the target inside this crop"));
}

#[test]
fn grounding_helpers_extract_json_and_convert_bounding_boxes() {
    let wrapped = "```json\n{\"status\":\"resolved\",\"found\":true}\n```";
    assert_eq!(
        extract_grounding_json(wrapped),
        Some("{\"status\":\"resolved\",\"found\":true}")
    );

    let rect = grounding_bbox_to_rect(&GroundingBoundingBox {
        x1: 10.0,
        y1: 20.0,
        x2: 30.0,
        y2: 60.0,
    })
    .expect("bbox should convert");
    assert_eq!(rect.x, 10.0);
    assert_eq!(rect.y, 20.0);
    assert_eq!(rect.width, 20.0);
    assert_eq!(rect.height, 40.0);
    assert!(
        grounding_bbox_to_rect(&GroundingBoundingBox {
            x1: 30.0,
            y1: 20.0,
            x2: 10.0,
            y2: 60.0,
        })
        .is_none()
    );
}

#[test]
fn grounding_validation_overlay_preserves_image_dimensions() {
    let base = image::RgbaImage::from_pixel(32, 24, image::Rgba([240, 240, 240, 255]));
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(base.clone())
        .write_to(&mut encoded, ImageFormat::Png)
        .expect("base image should encode");
    let base_png = encoded.into_inner();

    let overlay = render_validation_overlay(
        &base_png,
        &HelperPoint { x: 12.0, y: 8.0 },
        Some(&GroundingBoundingBox {
            x1: 4.0,
            y1: 5.0,
            x2: 20.0,
            y2: 16.0,
        }),
    )
    .expect("overlay should render");
    let rendered = image::load_from_memory(&overlay).expect("overlay should decode");

    assert_eq!(rendered.width(), 32);
    assert_eq!(rendered.height(), 24);
    assert_ne!(overlay, base_png);
}

#[test]
fn grounding_guide_overlay_preserves_image_dimensions() {
    let base = image::RgbaImage::from_pixel(32, 24, image::Rgba([240, 240, 240, 255]));
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(base.clone())
        .write_to(&mut encoded, ImageFormat::Png)
        .expect("base image should encode");
    let base_png = encoded.into_inner();

    let overlay = render_guide_overlay(
        &base_png,
        &HelperPoint { x: 14.0, y: 10.0 },
        Some(&GroundingBoundingBox {
            x1: 6.0,
            y1: 4.0,
            x2: 18.0,
            y2: 20.0,
        }),
    )
    .expect("guide overlay should render");
    let rendered = image::load_from_memory(&overlay).expect("guide overlay should decode");

    assert_eq!(rendered.width(), 32);
    assert_eq!(rendered.height(), 24);
    assert_ne!(overlay, base_png);
}

#[test]
fn annotate_grounding_raw_adds_validation_metadata() {
    let mut raw = serde_json::json!({
        "status": "resolved",
        "found": true,
    });

    annotate_grounding_raw(
        &mut raw,
        "complex",
        "validated",
        true,
        "approved",
        Some("marker aligned with the target"),
        Some(0.92),
        2,
    );

    assert_eq!(raw["grounding_mode_requested"], "complex");
    assert_eq!(raw["grounding_mode_effective"], "complex");
    assert_eq!(raw["selected_attempt"], "validated");
    assert_eq!(raw["grounding_validation_triggered"], true);
    assert_eq!(raw["grounding_rounds_attempted"], 2);
    assert_eq!(raw["validation"]["status"], "approved");
    assert_eq!(
        raw["validation"]["reason"],
        "marker aligned with the target"
    );
}

#[test]
fn annotate_grounding_round_artifacts_adds_structured_attempts() {
    let mut raw = serde_json::json!({
        "status": "resolved",
        "found": true,
    });

    annotate_grounding_round_artifacts(
        &mut raw,
        &[
            serde_json::json!({
                "round": 1,
                "terminal_state": "validator_rejected_retry",
                "predictor": {
                    "outcome": {
                        "status": "resolved",
                    },
                },
            }),
            serde_json::json!({
                "round": 2,
                "terminal_state": "accepted",
                "validation": {
                    "triggered": true,
                    "outcome": {
                        "status": "approved",
                    },
                },
            }),
        ],
    );

    assert_eq!(raw["grounding_round_artifacts"][0]["round"], 1);
    assert_eq!(
        raw["grounding_round_artifacts"][0]["terminal_state"],
        "validator_rejected_retry"
    );
    assert_eq!(raw["grounding_round_artifacts"][1]["round"], 2);
    assert_eq!(
        raw["grounding_round_artifacts"][1]["validation"]["triggered"],
        true
    );
}

#[test]
fn target_resolution_details_surface_grounding_diagnostics_summary() {
    let grounded = GroundedGuiTarget {
        grounding_method: "grounding",
        resolved: ResolvedTarget {
            window_title: Some("Finder".to_string()),
            provider: "grok-build:gui-grounding".to_string(),
            confidence: 0.92,
            reason: Some("matched visible search affordance".to_string()),
            grounding_mode_requested: "complex".to_string(),
            grounding_mode_effective: "complex".to_string(),
            scope: Some("toolbar".to_string()),
            point: HelperPoint { x: 1200.0, y: 42.0 },
            bounds: HelperRect {
                x: 1180.0,
                y: 22.0,
                width: 48.0,
                height: 32.0,
            },
            local_point: Some(HelperPoint { x: 600.0, y: 21.0 }),
            local_bounds: Some(HelperRect {
                x: 590.0,
                y: 11.0,
                width: 24.0,
                height: 16.0,
            }),
            raw: Some(serde_json::json!({
                "selected_attempt": "validated_retry",
                "grounding_rounds_attempted": 2,
                "grounding_validation_triggered": true,
                "grounding_model_image": {
                    "model_width": 800,
                    "model_height": 600,
                },
                "validation": {
                    "status": "approved",
                },
                "grounding_round_artifacts": [
                    {
                        "round": 1,
                        "terminal_state": "validator_rejected_retry",
                    },
                    {
                        "round": 2,
                        "terminal_state": "accepted",
                    },
                ],
            })),
            capture_state: ObserveState {
                capture: CaptureArtifact {
                    origin_x: 0.0,
                    origin_y: 0.0,
                    width: 1440,
                    height: 900,
                    image_width: 1440,
                    image_height: 900,
                    display_index: 1,
                    capture_mode: CaptureMode::Display,
                    window_title: Some("Finder".to_string()),
                    window_count: Some(1),
                    window_capture_strategy: Some("display".to_string()),
                    host_exclusion: HostCaptureExclusionState::default(),
                },
                app_name: Some("Finder".to_string()),
            },
        },
    };

    let details = build_target_resolution_details("Search", &grounded);

    assert_eq!(
        details["grounding_diagnostics"]["selected_attempt"],
        "validated_retry"
    );
    assert_eq!(details["grounding_diagnostics"]["rounds_attempted"], 2);
    assert_eq!(
        details["grounding_diagnostics"]["round_artifacts"][0]["terminal_state"],
        "validator_rejected_retry"
    );
    assert_eq!(
        details["grounding_diagnostics"]["model_image"]["model_width"],
        800
    );
}

// ---------------------------------------------------------------------------
// From upstream gui/tests/coordinate_space.rs
// ---------------------------------------------------------------------------

#[test]
fn local_point_within_state_reports_only_in_bounds_targets() {
    let state = ObserveState {
        capture: CaptureArtifact {
            origin_x: 100.0,
            origin_y: 200.0,
            width: 400,
            height: 300,
            image_width: 800,
            image_height: 600,
            display_index: 1,
            capture_mode: CaptureMode::Window,
            window_title: Some("Quick Note".to_string()),
            window_count: Some(1),
            window_capture_strategy: Some("selected_window".to_string()),
            host_exclusion: HostCaptureExclusionState::default(),
        },
        app_name: Some("Notes".to_string()),
    };

    let in_bounds = local_point_within_state(&state, &HelperPoint { x: 125.0, y: 250.0 })
        .expect("point should be within capture");
    assert_eq!(in_bounds.x, 25.0);
    assert_eq!(in_bounds.y, 50.0);
    assert!(local_point_within_state(&state, &HelperPoint { x: 50.0, y: 250.0 }).is_none());
}

#[test]
fn image_space_helpers_validate_capture_relative_geometry() {
    let state = ObserveState {
        capture: CaptureArtifact {
            origin_x: 100.0,
            origin_y: 200.0,
            width: 400,
            height: 300,
            image_width: 800,
            image_height: 600,
            display_index: 1,
            capture_mode: CaptureMode::Window,
            window_title: Some("Quick Note".to_string()),
            window_count: Some(1),
            window_capture_strategy: Some("selected_window".to_string()),
            host_exclusion: HostCaptureExclusionState::default(),
        },
        app_name: Some("Notes".to_string()),
    };

    let point = image_point_within_capture(&state, &HelperPoint { x: 125.0, y: 250.0 })
        .expect("point should remain in image space");
    assert_eq!(point.x, 125.0);
    assert_eq!(point.y, 250.0);
    assert!(image_point_within_capture(&state, &HelperPoint { x: 850.0, y: 250.0 }).is_none());

    let rect = local_rect_within_state(
        &state,
        &HelperRect {
            x: 10.0,
            y: 20.0,
            width: 100.0,
            height: 80.0,
        },
    )
    .expect("rect should fit inside capture");
    assert_eq!(rect.width, 100.0);
    assert_eq!(rect.height, 80.0);
    // Rect that extends past the right edge should be clamped, not rejected.
    let clamped = local_rect_within_state(
        &state,
        &HelperRect {
            x: 750.0,
            y: 20.0,
            width: 100.0,
            height: 80.0,
        },
    )
    .expect("rect with origin inside image should be clamped");
    assert_eq!(clamped.x, 750.0);
    assert_eq!(clamped.width, 50.0); // 800 - 750
    assert_eq!(clamped.height, 80.0);

    // Rect with origin completely outside the image should still be rejected.
    assert!(
        local_rect_within_state(
            &state,
            &HelperRect {
                x: 850.0,
                y: 20.0,
                width: 100.0,
                height: 80.0,
            },
        )
        .is_none()
    );
}

#[test]
fn coordinate_helpers_require_complete_pairs_and_valid_spaces() {
    assert_eq!(
        normalize_coordinate_space(None).expect("default coordinate space"),
        GuiCoordinateSpace::ImagePixels,
    );
    assert_eq!(
        normalize_coordinate_space(Some("display_points")).expect("display space"),
        GuiCoordinateSpace::DisplayPoints,
    );
    assert!(normalize_coordinate_space(Some("screen_pixels")).is_err());

    let point = normalize_optional_coordinate_point(Some(12.0), Some(34.0), "x", "y")
        .expect("coordinate pair")
        .expect("point");
    assert_eq!(point.x, 12.0);
    assert_eq!(point.y, 34.0);
    assert!(normalize_optional_coordinate_point(Some(12.0), None, "x", "y").is_err());
}

#[test]
fn image_space_geometry_maps_back_to_display_space() {
    let state = ObserveState {
        capture: CaptureArtifact {
            origin_x: 100.0,
            origin_y: 200.0,
            width: 400,
            height: 300,
            image_width: 800,
            image_height: 600,
            display_index: 1,
            capture_mode: CaptureMode::Window,
            window_title: Some("Quick Note".to_string()),
            window_count: Some(1),
            window_capture_strategy: Some("selected_window".to_string()),
            host_exclusion: HostCaptureExclusionState::default(),
        },
        app_name: Some("Notes".to_string()),
    };

    let display_point = image_point_to_display(&state, &HelperPoint { x: 300.0, y: 120.0 });
    assert_eq!(display_point.x, 250.0);
    assert_eq!(display_point.y, 260.0);

    let display_rect = image_rect_to_display(
        &state,
        &HelperRect {
            x: 200.0,
            y: 100.0,
            width: 160.0,
            height: 80.0,
        },
    );
    assert_eq!(display_rect.x, 200.0);
    assert_eq!(display_rect.y, 250.0);
    assert_eq!(display_rect.width, 80.0);
    assert_eq!(display_rect.height, 40.0);
}

#[test]
fn grounding_model_image_normalizes_hidpi_capture_dimensions() {
    let base = image::RgbaImage::from_pixel(1600, 1200, image::Rgba([240, 240, 240, 255]));
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(base)
        .write_to(&mut encoded, ImageFormat::Png)
        .expect("base image should encode");

    let prepared = prepare_grounding_model_image(
        &encoded.into_inner(),
        GroundingModelImageConfig {
            logical_width: Some(800),
            logical_height: Some(600),
            scale_x: Some(2.0),
            scale_y: Some(2.0),
            allow_logical_normalization: true,
        },
        "test image",
    )
    .expect("prepared image should build");

    assert_eq!(prepared.original_width, 1600);
    assert_eq!(prepared.original_height, 1200);
    assert_eq!(prepared.working_width, 800);
    assert_eq!(prepared.working_height, 600);
    assert_eq!(prepared.model_width, 800);
    assert_eq!(prepared.model_height, 600);
    assert_eq!(prepared.mime_type, "image/png");
    assert!(prepared.logical_normalization_applied);
    assert!((prepared.model_to_original_scale_x - 2.0).abs() < f64::EPSILON);
    assert!((prepared.model_to_original_scale_y - 2.0).abs() < f64::EPSILON);
}

#[test]
fn grounding_model_image_roundtrips_between_model_and_original_space() {
    let base = image::RgbaImage::from_pixel(1600, 1200, image::Rgba([240, 240, 240, 255]));
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(base)
        .write_to(&mut encoded, ImageFormat::Png)
        .expect("base image should encode");
    let prepared = prepare_grounding_model_image(
        &encoded.into_inner(),
        GroundingModelImageConfig {
            logical_width: Some(800),
            logical_height: Some(600),
            scale_x: Some(2.0),
            scale_y: Some(2.0),
            allow_logical_normalization: true,
        },
        "test image",
    )
    .expect("prepared image should build");
    let original_point = HelperPoint { x: 640.0, y: 420.0 };

    let model_point = translate_original_point_to_model(&prepared, &original_point);
    let roundtrip_point = translate_model_point_to_original(&prepared, &model_point);

    assert!((model_point.x - 320.0).abs() < 0.01);
    assert!((model_point.y - 210.0).abs() < 0.01);
    assert!((roundtrip_point.x - original_point.x).abs() < 0.01);
    assert!((roundtrip_point.y - original_point.y).abs() < 0.01);
}

#[test]
fn refinement_is_used_for_tiny_targets_and_maps_back_to_original_space() {
    let state = ObserveState {
        capture: CaptureArtifact {
            origin_x: 0.0,
            origin_y: 0.0,
            width: 800,
            height: 600,
            image_width: 1600,
            image_height: 1200,
            display_index: 1,
            capture_mode: CaptureMode::Window,
            window_title: Some("Quick Note".to_string()),
            window_count: Some(1),
            window_capture_strategy: Some("selected_window".to_string()),
            host_exclusion: HostCaptureExclusionState::default(),
        },
        app_name: Some("Notes".to_string()),
    };
    let original_point = HelperPoint { x: 640.0, y: 420.0 };
    let tiny_bbox = GroundingBoundingBox {
        x1: 628.0,
        y1: 408.0,
        x2: 652.0,
        y2: 432.0,
    };

    assert!(should_use_high_resolution_refinement(
        &state,
        &original_point,
        Some(&tiny_bbox)
    ));

    let base = image::RgbaImage::from_pixel(1600, 1200, image::Rgba([240, 240, 240, 255]));
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(base)
        .write_to(&mut encoded, ImageFormat::Png)
        .expect("base image should encode");
    let crop = create_refinement_crop(&encoded.into_inner(), &original_point, Some(&tiny_bbox))
        .expect("refinement crop should build")
        .expect("refinement crop should exist");

    let refinement_point = translate_original_point_to_refinement(&crop, &original_point);
    let roundtrip_point = translate_refinement_point_to_original(&crop, &refinement_point);

    assert!((roundtrip_point.x - original_point.x).abs() < 0.51);
    assert!((roundtrip_point.y - original_point.y).abs() < 0.51);
    assert!(crop.model_width >= crop.crop_width);
    assert!(crop.model_height >= crop.crop_height);
}
