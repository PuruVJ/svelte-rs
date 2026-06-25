//! `bind:` property catalog.
//!
//! Ported from `packages/svelte/src/compiler/phases/bindings.js`. Lists
//! every DOM property name Svelte recognises as a valid `bind:` target,
//! together with element constraints. Consumed by the `BindDirective`
//! validator (in `validate.rs`) to surface `bind_invalid_target` /
//! `bind_invalid_name` errors when the user writes `bind:foo` on the
//! wrong element.

use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Debug, Clone, Default)]
pub struct BindingProperty {
    /// DOM event that notifies of changes to this property.
    pub event: Option<&'static str>,
    /// Updates are written back to the DOM property.
    pub bidirectional: bool,
    /// Should NOT be included in SSR output.
    pub omit_in_ssr: bool,
    /// If `Some`, the binding is only valid on these element names.
    pub valid_elements: Option<&'static [&'static str]>,
    /// If `Some`, the binding is invalid on these element names.
    pub invalid_elements: Option<&'static [&'static str]>,
}

/// Static lookup table.
pub fn binding_properties() -> &'static HashMap<&'static str, BindingProperty> {
    static MAP: OnceLock<HashMap<&'static str, BindingProperty>> = OnceLock::new();
    MAP.get_or_init(build)
}

fn build() -> HashMap<&'static str, BindingProperty> {
    let media: &[&str] = &["audio", "video"];
    let video: &[&str] = &["video"];
    let img: &[&str] = &["img"];
    let doc: &[&str] = &["svelte:document"];
    let win: &[&str] = &["svelte:window"];
    let win_doc: &[&str] = &["svelte:window", "svelte:document"];
    let input: &[&str] = &["input"];
    let details: &[&str] = &["details"];
    let inputs: &[&str] = &["input", "textarea", "select"];

    let mut m = HashMap::new();

    // ===== media =====
    m.insert("currentTime", BindingProperty {
        valid_elements: Some(media), omit_in_ssr: true, bidirectional: true,
        ..Default::default()
    });
    m.insert("duration", BindingProperty {
        valid_elements: Some(media), event: Some("durationchange"), omit_in_ssr: true,
        ..Default::default()
    });
    m.insert("focused", BindingProperty::default());
    m.insert("paused", BindingProperty {
        valid_elements: Some(media), omit_in_ssr: true, bidirectional: true,
        ..Default::default()
    });
    m.insert("buffered", BindingProperty {
        valid_elements: Some(media), omit_in_ssr: true, ..Default::default()
    });
    m.insert("seekable", BindingProperty {
        valid_elements: Some(media), omit_in_ssr: true, ..Default::default()
    });
    m.insert("played", BindingProperty {
        valid_elements: Some(media), omit_in_ssr: true, ..Default::default()
    });
    m.insert("volume", BindingProperty {
        valid_elements: Some(media), omit_in_ssr: true, bidirectional: true,
        ..Default::default()
    });
    m.insert("muted", BindingProperty {
        valid_elements: Some(media), omit_in_ssr: true, bidirectional: true,
        ..Default::default()
    });
    m.insert("playbackRate", BindingProperty {
        valid_elements: Some(media), omit_in_ssr: true, bidirectional: true,
        ..Default::default()
    });
    m.insert("seeking", BindingProperty {
        valid_elements: Some(media), omit_in_ssr: true, ..Default::default()
    });
    m.insert("ended", BindingProperty {
        valid_elements: Some(media), omit_in_ssr: true, ..Default::default()
    });
    m.insert("readyState", BindingProperty {
        valid_elements: Some(media), omit_in_ssr: true, ..Default::default()
    });

    // ===== video =====
    m.insert("videoHeight", BindingProperty {
        valid_elements: Some(video), event: Some("resize"), omit_in_ssr: true,
        ..Default::default()
    });
    m.insert("videoWidth", BindingProperty {
        valid_elements: Some(video), event: Some("resize"), omit_in_ssr: true,
        ..Default::default()
    });

    // ===== img =====
    m.insert("naturalWidth", BindingProperty {
        valid_elements: Some(img), event: Some("load"), omit_in_ssr: true,
        ..Default::default()
    });
    m.insert("naturalHeight", BindingProperty {
        valid_elements: Some(img), event: Some("load"), omit_in_ssr: true,
        ..Default::default()
    });

    // ===== svelte:document =====
    m.insert("activeElement", BindingProperty {
        valid_elements: Some(doc), omit_in_ssr: true, ..Default::default()
    });
    m.insert("fullscreenElement", BindingProperty {
        valid_elements: Some(doc), event: Some("fullscreenchange"), omit_in_ssr: true,
        ..Default::default()
    });
    m.insert("pointerLockElement", BindingProperty {
        valid_elements: Some(doc), event: Some("pointerlockchange"), omit_in_ssr: true,
        ..Default::default()
    });
    m.insert("visibilityState", BindingProperty {
        valid_elements: Some(doc), event: Some("visibilitychange"), omit_in_ssr: true,
        ..Default::default()
    });

    // ===== svelte:window =====
    m.insert("innerWidth", BindingProperty {
        valid_elements: Some(win), omit_in_ssr: true, ..Default::default()
    });
    m.insert("innerHeight", BindingProperty {
        valid_elements: Some(win), omit_in_ssr: true, ..Default::default()
    });
    m.insert("outerWidth", BindingProperty {
        valid_elements: Some(win), omit_in_ssr: true, ..Default::default()
    });
    m.insert("outerHeight", BindingProperty {
        valid_elements: Some(win), omit_in_ssr: true, ..Default::default()
    });
    m.insert("scrollX", BindingProperty {
        valid_elements: Some(win), omit_in_ssr: true, bidirectional: true,
        ..Default::default()
    });
    m.insert("scrollY", BindingProperty {
        valid_elements: Some(win), omit_in_ssr: true, bidirectional: true,
        ..Default::default()
    });
    m.insert("online", BindingProperty {
        valid_elements: Some(win), omit_in_ssr: true, ..Default::default()
    });
    m.insert("devicePixelRatio", BindingProperty {
        valid_elements: Some(win), event: Some("resize"), omit_in_ssr: true,
        ..Default::default()
    });

    // ===== dimensions (every element except window/document) =====
    m.insert("clientWidth", BindingProperty {
        omit_in_ssr: true, invalid_elements: Some(win_doc), ..Default::default()
    });
    m.insert("clientHeight", BindingProperty {
        omit_in_ssr: true, invalid_elements: Some(win_doc), ..Default::default()
    });
    m.insert("offsetWidth", BindingProperty {
        omit_in_ssr: true, invalid_elements: Some(win_doc), ..Default::default()
    });
    m.insert("offsetHeight", BindingProperty {
        omit_in_ssr: true, invalid_elements: Some(win_doc), ..Default::default()
    });
    m.insert("contentRect", BindingProperty {
        omit_in_ssr: true, invalid_elements: Some(win_doc), ..Default::default()
    });
    m.insert("contentBoxSize", BindingProperty {
        omit_in_ssr: true, invalid_elements: Some(win_doc), ..Default::default()
    });
    m.insert("borderBoxSize", BindingProperty {
        omit_in_ssr: true, invalid_elements: Some(win_doc), ..Default::default()
    });
    m.insert("devicePixelContentBoxSize", BindingProperty {
        omit_in_ssr: true, invalid_elements: Some(win_doc), ..Default::default()
    });

    // ===== input =====
    m.insert("indeterminate", BindingProperty {
        event: Some("change"), bidirectional: true, valid_elements: Some(input),
        omit_in_ssr: true, ..Default::default()
    });
    m.insert("checked", BindingProperty {
        valid_elements: Some(input), bidirectional: true, ..Default::default()
    });
    m.insert("group", BindingProperty {
        valid_elements: Some(input), bidirectional: true, ..Default::default()
    });
    m.insert("files", BindingProperty {
        valid_elements: Some(input), omit_in_ssr: true, bidirectional: true,
        ..Default::default()
    });

    // ===== various =====
    m.insert("this", BindingProperty {
        omit_in_ssr: true, ..Default::default()
    });
    m.insert("innerText", BindingProperty {
        invalid_elements: Some(win_doc), bidirectional: true, ..Default::default()
    });
    m.insert("innerHTML", BindingProperty {
        invalid_elements: Some(win_doc), bidirectional: true, ..Default::default()
    });
    m.insert("textContent", BindingProperty {
        invalid_elements: Some(win_doc), bidirectional: true, ..Default::default()
    });
    m.insert("open", BindingProperty {
        event: Some("toggle"), bidirectional: true, valid_elements: Some(details),
        ..Default::default()
    });
    m.insert("value", BindingProperty {
        valid_elements: Some(inputs), bidirectional: true, ..Default::default()
    });

    m
}
