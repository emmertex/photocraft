//! File › Save writes back to the file a document was opened from, in that file's format,
//! keeping its bit depth, ICC profile, EXIF and XMP; layers going into a flat file are confirmed
//! first. External editors (a DAM's "Edit in", `--wait` scripts) depend on this.

use crate::{PhotocraftApp, Services, flatten_save_ui, menus};
use photocraft_codecs::{ChannelLayout, Format, Image, Metadata, SampleType};
use photocraft_engine::Session;
use serde_json::{Value, json};
use std::{cell::RefCell, rc::Rc};

type Writes = Rc<RefCell<Vec<(String, Vec<u8>)>>>;

/// EXIF with one IFD0 entry (Make = "PC"), big-endian.
const EXIF: &[u8] = b"MM\0*\0\0\0\x08\0\x01\x01\x0f\0\x02\0\0\0\x03PC\0\0\0\0\0\0";
const XMP_MARK: &str = "external-edit-round-trip";

fn xmp() -> String {
    format!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?><x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description rdf:about=\"\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><dc:source>{XMP_MARK}</dc:source></rdf:Description></rdf:RDF></x:xmpmeta><?xpacket end=\"w\"?>"
    )
}

fn p3() -> Vec<u8> {
    photocraft_cms::Builtin::DisplayP3.profile().to_bytes().to_vec()
}

fn app(writes: &Writes) -> PhotocraftApp {
    let written = writes.clone();
    PhotocraftApp::new(
        Session::new(),
        Services {
            import: Some(Box::new(|name, bytes, depth| {
                photocraft_io::import_with_svg_group_depth(name, bytes, depth).map(|r| (r.document, r.warnings)).map_err(|e| e.to_string())
            })),
            export: Some(Box::new(|doc, path, settings| {
                let opts = photocraft_io::ExportOptions { tiff_layers: settings.tiff_layers, ..Default::default() };
                photocraft_io::export(doc, path, &opts).map(|r| (r.bytes, r.warnings)).map_err(|e| e.to_string())
            })),
            write: Some(Box::new(move |path, bytes| {
                written.borrow_mut().push((path.to_string(), bytes.to_vec()));
                Ok(())
            })),
            automation_write: Some(Box::new(|_, _| Err("automation writer used".into()))),
            ..Default::default()
        },
    )
}

/// A 6×4 RGB source file in `format` at `sample`, with a Display P3 profile, EXIF and XMP.
fn source(format: Format, sample: SampleType) -> Vec<u8> {
    let values: Vec<f32> = (0..6 * 4 * 3).map(|i| (i % 17) as f32 / 16.0).collect();
    let meta = Metadata { exif: Some(EXIF.to_vec()), xmp: Some(xmp()), ..Default::default() };
    let img = Image::from_normalized(6, 4, ChannelLayout::Rgb, sample, &values).unwrap().with_icc(Some(p3())).with_meta(meta);
    photocraft_codecs::encode(&img, format, &Default::default()).unwrap()
}

/// Opens `bytes` as the file at `path`, as File › Open does.
fn open(app: &mut PhotocraftApp, path: &str, bytes: &[u8]) {
    app.open_bytes(path.rsplit('/').next().unwrap(), bytes).unwrap();
    app.session.active_mut().unwrap().path = Some(path.to_string());
}

fn dirty(app: &PhotocraftApp) -> bool {
    app.session.active().unwrap().is_dirty()
}

fn save(app: &mut PhotocraftApp, params: Value) -> Result<Value, String> {
    menus::invoke(app, &egui::Context::default(), "file.save", params)
}

#[test]
fn plain_flat_files_save_in_place_keeping_depth_profile_and_metadata() {
    let cases = [
        ("tif", Format::Tiff, vec![SampleType::U8, SampleType::U16, SampleType::F32]),
        ("png", Format::Png, vec![SampleType::U8, SampleType::U16]),
        ("jpg", Format::Jpeg, vec![SampleType::U8]),
        ("webp", Format::WebP, vec![SampleType::U8]),
    ];
    for (ext, format, samples) in cases {
        for sample in samples {
            let writes = Writes::default();
            let mut app = app(&writes);
            let path = format!("photos/shot.{ext}");
            open(&mut app, &path, &source(format, sample));
            app.run("image.adjustments.invert", json!({})).unwrap();
            assert!(dirty(&app));
            let r = save(&mut app, json!({})).unwrap();
            assert_eq!(r["path"], path, "{ext} {sample:?}: {r}");
            assert!(app.flatten_save.is_none(), "a plain image needs no confirmation");
            assert!(!dirty(&app), "{ext} {sample:?}: saved");
            assert_eq!(app.session.active().unwrap().path.as_deref(), Some(path.as_str()));
            let writes = writes.borrow();
            assert_eq!(writes.len(), 1);
            assert_eq!(writes[0].0, path);
            let back = photocraft_codecs::decode(&writes[0].1).unwrap();
            assert_eq!(photocraft_codecs::detect(&writes[0].1), Some(format), "{ext}: written in its own format");
            assert_eq!(back.sample_type(), sample, "{ext}: bit depth kept");
            assert_eq!(back.icc.as_deref(), Some(p3().as_slice()), "{ext} {sample:?}: ICC profile kept");
            assert!(back.meta.xmp.as_deref().is_some_and(|x| x.contains(XMP_MARK)), "{ext} {sample:?}: XMP kept");
            // EXIF wherever the format carries it (the TIFF codec has no EXIF IFD support yet).
            if format.caps().exif {
                let exif = back.meta.exif.as_deref().unwrap_or_else(|| panic!("{ext} {sample:?}: EXIF kept"));
                assert!(exif.windows(2).any(|w| w == b"PC"), "{ext} {sample:?}: EXIF Make kept");
            }
            // The pixels are the edited ones.
            assert!(back.sample_normalized(0) > 0.9, "{ext} {sample:?}: inverted pixels written");
        }
    }
}

#[test]
fn layered_documents_confirm_before_flattening_into_their_flat_file() {
    for ext in ["png", "jpg", "webp"] {
        let format = photocraft_codecs::from_extension(ext).unwrap();
        let writes = Writes::default();
        let mut app = app(&writes);
        let path = format!("shot.{ext}");
        open(&mut app, &path, &source(format, SampleType::U8));
        app.run("layer.new.layer", json!({})).unwrap();
        let r = save(&mut app, json!({})).unwrap();
        assert_eq!(r["prompt"], "flatten", "{r}");
        assert!(writes.borrow().is_empty(), "nothing written before the answer");
        assert_eq!(flatten_save_ui::inspect(&app)["path"], path);
        assert!(save(&mut app, json!({})).is_err(), "a second save waits for the prompt");

        let ctx = egui::Context::default();
        flatten_save_ui::answer(&mut app, &ctx, flatten_save_ui::Answer::Cancel);
        assert!(app.flatten_save.is_none() && writes.borrow().is_empty() && dirty(&app), "Cancel writes nothing");

        save(&mut app, json!({})).unwrap();
        flatten_save_ui::answer(&mut app, &ctx, flatten_save_ui::Answer::Flatten);
        assert_eq!(writes.borrow().len(), 1);
        assert_eq!(writes.borrow()[0].0, path);
        assert!(!dirty(&app), "the flattened file is the document's saved state");
        assert_eq!(app.session.active().unwrap().doc.layers.len(), 2, "the layers stay in the window");
        assert_eq!(app.session.journal.last().unwrap().1, json!({"flatten": true}), "recorded with its answer");

        // A script answers in advance.
        app.run("layer.new.layer", json!({})).unwrap();
        save(&mut app, json!({"flatten": true})).unwrap();
        assert!(app.flatten_save.is_none());
        assert_eq!(writes.borrow().len(), 2);
    }
}

#[test]
fn tiffs_keep_their_layers_in_place_unless_the_preference_is_off() {
    let writes = Writes::default();
    let mut app = app(&writes);
    open(&mut app, "layers.tif", &source(Format::Tiff, SampleType::U16));
    app.run("layer.new.layer", json!({})).unwrap();
    save(&mut app, json!({})).unwrap();
    assert!(app.flatten_save.is_none() && app.tiff_options.is_none(), "Save Layers in TIFF is on by default: no prompt");
    assert!(!dirty(&app));
    let bytes = writes.borrow()[0].1.clone();
    let reopened = photocraft_io::import("layers.tif", &bytes).unwrap().document;
    assert_eq!(reopened.layers.len(), 2, "layers written into the TIFF");
    assert_eq!(reopened.depth.bits(), 16);

    app.session.edit_prefs(|p| p.file_handling.save_layers_in_tiff = false);
    app.run("layer.new.layer", json!({})).unwrap();
    let r = save(&mut app, json!({})).unwrap();
    assert_eq!(r["prompt"], "flatten", "{r}");
    flatten_save_ui::answer(&mut app, &egui::Context::default(), flatten_save_ui::Answer::Flatten);
    let reopened = photocraft_io::import("layers.tif", &writes.borrow()[1].1).unwrap().document;
    assert_eq!(reopened.layers.len(), 1, "flattened");
}

#[test]
fn read_only_or_unknown_formats_go_through_save_as() {
    for path in ["shot.gif", "raw.cr2", "noext"] {
        let writes = Writes::default();
        let mut app = app(&writes);
        app.run("file.new", json!({"width": 4, "height": 4})).unwrap();
        app.session.active_mut().unwrap().path = Some(path.into());
        // No file dialog service here: Save As fails rather than writing over the file.
        let _ = save(&mut app, json!({}));
        assert!(writes.borrow().is_empty(), "{path}: never written over");
        assert!(app.flatten_save.is_none());
    }
}

#[test]
fn malformed_files_and_bad_params_fail_without_writing() {
    let writes = Writes::default();
    let mut app = app(&writes);
    assert!(app.open_bytes("broken.jpg", b"\xff\xd8\xff\xe0 not really a jpeg").is_err());
    assert!(save(&mut app, json!({})).is_err(), "no document");
    open(&mut app, "ok.png", &source(Format::Png, SampleType::U8));
    // A non-boolean `flatten` is "not confirmed", never a crash.
    app.run("layer.new.layer", json!({})).unwrap();
    for flatten in [json!("yes"), json!(1), json!(null), json!([true])] {
        app.flatten_save = None;
        let r = save(&mut app, json!({"flatten": flatten})).unwrap();
        assert_eq!(r["prompt"], "flatten");
    }
    assert!(writes.borrow().is_empty());
}
