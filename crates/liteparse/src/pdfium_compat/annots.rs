//! Annotations, links and AcroForm widgets, read from the page's `/Annots`.

use std::collections::HashSet;

use hayro::hayro_syntax::content::TypedIter;
use hayro::hayro_syntax::content::ops::TypedInstruction;
use hayro::hayro_syntax::object::{Array, Dict, Name, Object, Stream};

use super::content::normal_appearance;
use super::document::FormEnvironment;
use super::objects::{catalog, decode_text_string, dict_text, dict_text_or_name};
use super::page::Page;
use super::types::RectF;

/// A URI hyperlink annotation. `rect` is in viewport space.
#[derive(Debug, Clone)]
pub struct PdfLink {
    pub rect: RectF,
    pub uri: String,
}

/// One PDF annotation with geometry in viewport space. String fields are
/// `None` when the key is absent and `Some("")` when present but empty.
#[derive(Debug, Clone)]
pub struct PdfAnnotation {
    pub object_number: Option<i32>,
    pub subtype: String,
    pub contents: Option<String>,
    pub created: Option<String>,
    pub modified: Option<String>,
    pub title: Option<String>,
    pub rect: Option<RectF>,
    /// Whether the subtype carries attachment points (links and the
    /// text-markup subtypes), as `FPDFAnnot_HasAttachmentPoints` reports.
    pub has_attachment_points: bool,
    pub quadpoint_rects: Vec<RectF>,
    pub uri: Option<String>,
    pub uri_raw: Option<Vec<u8>>,
}

/// One AcroForm widget with its resolved field metadata.
#[derive(Debug, Clone)]
pub struct PdfFormField {
    pub id: String,
    pub field_type: String,
    pub page: u32,
    pub annotation_index: i32,
    pub widget_index: i32,
    pub object_number: Option<i32>,
    pub name: Option<String>,
    pub alternate_name: Option<String>,
    pub value: Option<String>,
    pub export_value: Option<String>,
    pub field_flags: i32,
    pub control_count: Option<i32>,
    pub control_index: Option<i32>,
    pub checked: Option<bool>,
    pub rect: Option<RectF>,
    pub options: Vec<String>,
    pub selected_options: Vec<String>,
}

const FLAG_INVISIBLE: i32 = 1;
const FLAG_HIDDEN: i32 = 2;
const FLAG_NOVIEW: i32 = 32;

/// Field flags (PDF 32000 §12.7.4).
const FF_RADIO: i32 = 1 << 15;
const FF_PUSHBUTTON: i32 = 1 << 16;
const FF_COMBO: i32 = 1 << 17;

/// `(annotation dict, object number)` for every entry of `/Annots`.
fn page_annotations<'a>(page: &Dict<'a>) -> Vec<(Dict<'a>, Option<i32>)> {
    let Some(annots) = page.get::<Array<'_>>(b"Annots") else {
        return Vec::new();
    };
    annots
        .raw_iter()
        .zip(annots.iter::<Object<'_>>())
        .filter_map(|(raw, object)| {
            let number = raw.as_obj_ref().map(|r| r.obj_number);
            match object {
                Object::Dict(dict) => Some((dict, number)),
                _ => None,
            }
        })
        .collect()
}

fn subtype(annot: &Dict<'_>) -> Vec<u8> {
    annot
        .get::<Name<'_>>(b"Subtype")
        .map(|n| n.as_ref().to_vec())
        .unwrap_or_default()
}

fn subtype_name(subtype: &[u8]) -> &'static str {
    match subtype {
        b"Text" => "text",
        b"Link" => "link",
        b"FreeText" => "freetext",
        b"Line" => "line",
        b"Square" => "square",
        b"Circle" => "circle",
        b"Polygon" => "polygon",
        b"PolyLine" => "polyline",
        b"Highlight" => "highlight",
        b"Underline" => "underline",
        b"Squiggly" => "squiggly",
        b"StrikeOut" => "strikeout",
        b"Stamp" => "stamp",
        b"Caret" => "caret",
        b"Ink" => "ink",
        b"Popup" => "popup",
        b"FileAttachment" => "fileattachment",
        b"Sound" => "sound",
        b"Movie" => "movie",
        b"Widget" => "widget",
        b"Screen" => "screen",
        b"PrinterMark" => "printermark",
        b"TrapNet" => "trapnet",
        b"Watermark" => "watermark",
        b"3D" => "threed",
        b"RichMedia" => "richmedia",
        b"XFAWidget" => "xfawidget",
        b"Redact" => "redact",
        _ => "unknown",
    }
}

fn flags(annot: &Dict<'_>) -> i32 {
    annot.get::<i32>(b"F").unwrap_or(0)
}

/// Present-but-empty strings stay `Some("")`.
fn string_if_present(annot: &Dict<'_>, key: &[u8]) -> Option<String> {
    annot
        .contains_key(key)
        .then(|| dict_text(annot, key).unwrap_or_default())
}

/// The link's URI action bytes, with a relative URI resolved against the
/// catalog's `/URI /Base` like `CPDF_Action::GetURI`.
fn uri_bytes(page: &Page<'_, '_>, annot: &Dict<'_>) -> Option<Vec<u8>> {
    let action = annot.get::<Dict<'_>>(b"A")?;
    if action.get::<Name<'_>>(b"S").as_deref() != Some(&b"URI"[..]) {
        return None;
    }
    let uri = action
        .get::<hayro::hayro_syntax::object::String<'_>>(b"URI")
        .map(|s| s.as_bytes().to_vec())
        .unwrap_or_default();
    if !uri.contains(&b':')
        && let Some(base) = catalog(page.document().pdf())
            .and_then(|root| root.get::<Dict<'_>>(b"URI"))
            .and_then(|uri_dict| uri_dict.get::<hayro::hayro_syntax::object::String<'_>>(b"Base"))
    {
        let mut full = base.as_bytes().to_vec();
        full.extend_from_slice(&uri);
        return Some(full);
    }
    Some(uri)
}

fn decode_uri(bytes: &[u8]) -> Option<String> {
    let uri = String::from_utf8_lossy(bytes)
        .trim_matches(char::from(0))
        .to_string();
    (!uri.is_empty()).then_some(uri)
}

fn quad_bounds(quads: &[f32]) -> Vec<RectF> {
    quads
        .chunks_exact(8)
        .map(|q| {
            let xs = [q[0], q[2], q[4], q[6]];
            let ys = [q[1], q[3], q[5], q[7]];
            RectF {
                left: xs.iter().copied().fold(f32::INFINITY, f32::min),
                bottom: ys.iter().copied().fold(f32::INFINITY, f32::min),
                right: xs.iter().copied().fold(f32::NEG_INFINITY, f32::max),
                top: ys.iter().copied().fold(f32::NEG_INFINITY, f32::max),
            }
        })
        .collect()
}

fn annot_rect(annot: &Dict<'_>) -> Option<RectF> {
    let r = annot.get::<[f32; 4]>(b"Rect")?;
    Some(RectF {
        left: r[0].min(r[2]),
        bottom: r[1].min(r[3]),
        right: r[0].max(r[2]),
        top: r[1].max(r[3]),
    })
}

/// Whether a content stream shows text; with `deep`, also inside the form
/// XObjects it draws.
fn stream_paints_text(
    stream: &Stream<'_>,
    deep: bool,
    depth: u32,
    seen: &mut HashSet<Vec<u8>>,
) -> bool {
    const MAX_DEPTH: u32 = 8;
    if !seen.insert(stream.dict().data().to_vec()) {
        return false;
    }
    let Ok(data) = stream.decoded() else {
        return false;
    };
    let resources = stream.dict().get::<Dict<'_>>(b"Resources");
    let mut ops = TypedIter::new(&data);
    while let Some(op) = ops.next() {
        match op {
            TypedInstruction::ShowText(_)
            | TypedInstruction::ShowTexts(_)
            | TypedInstruction::NextLineAndShowText(_)
            | TypedInstruction::ShowTextWithParameters(_) => return true,
            TypedInstruction::XObject(x) if deep && depth < MAX_DEPTH => {
                let Some(form) = resources
                    .as_ref()
                    .and_then(|res| res.get::<Dict<'_>>(b"XObject"))
                    .and_then(|xobjects| xobjects.get::<Stream<'_>>(x.0.as_ref()))
                    .filter(|s| {
                        s.dict().get::<Name<'_>>(b"Subtype").as_deref() == Some(&b"Form"[..])
                    })
                else {
                    continue;
                };
                if stream_paints_text(&form, deep, depth + 1, seen) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

fn appearance_paints_text(annot: &Dict<'_>, deep: bool) -> bool {
    normal_appearance(annot).is_some_and(|ap| stream_paints_text(&ap, deep, 0, &mut HashSet::new()))
}

/// Widgets a viewer paints that show text through their appearance.
fn text_widgets<'a>(page: &Dict<'a>) -> Vec<Dict<'a>> {
    page_annotations(page)
        .into_iter()
        .map(|(annot, _)| annot)
        .filter(|annot| {
            subtype(annot) == b"Widget"
                && flags(annot) & (FLAG_INVISIBLE | FLAG_HIDDEN | FLAG_NOVIEW) == 0
                && appearance_paints_text(annot, true)
        })
        .collect()
}

/// A field-tree attribute, inherited from the nearest ancestor that has it.
fn inherited<'a, T>(
    field: &Dict<'a>,
    key: &[u8],
    get: impl Fn(&Dict<'a>, &[u8]) -> Option<T>,
) -> Option<T> {
    let mut current = Some(field.clone());
    let mut depth = 0;
    while let Some(dict) = current {
        if let Some(value) = get(&dict, key) {
            return Some(value);
        }
        depth += 1;
        if depth > 32 {
            break;
        }
        current = dict.get::<Dict<'_>>(b"Parent");
    }
    None
}

/// `CPDF_FormField::GetFullName`: partial names joined with dots.
fn full_name(field: &Dict<'_>) -> Option<String> {
    let mut parts = Vec::new();
    let mut current = Some(field.clone());
    let mut depth = 0;
    while let Some(dict) = current {
        if let Some(part) = dict_text(&dict, b"T") {
            parts.push(part);
        }
        depth += 1;
        if depth > 32 {
            break;
        }
        current = dict.get::<Dict<'_>>(b"Parent");
    }
    parts.reverse();
    let name = parts.join(".");
    (!name.is_empty()).then_some(name)
}

/// The field a widget belongs to: the widget itself when it carries `/T` or
/// `/FT`, else its parent.
fn field_of<'a>(widget: &Dict<'a>) -> Dict<'a> {
    if widget.contains_key(b"T") || widget.contains_key(b"FT") {
        return widget.clone();
    }
    widget
        .get::<Dict<'_>>(b"Parent")
        .unwrap_or_else(|| widget.clone())
}

fn first_present<const N: usize>(values: [Option<String>; N]) -> Option<String> {
    values.into_iter().flatten().find(|value| !value.is_empty())
}

/// The name of a checkable widget's "on" appearance state.
fn on_state(widget: &Dict<'_>) -> Option<String> {
    let normal = widget.get::<Dict<'_>>(b"AP")?.get::<Dict<'_>>(b"N")?;
    normal
        .keys()
        .map(|k| k.as_str().to_string())
        .find(|k| k != "Off")
}

impl Page<'_, '_> {
    fn view_rect(&self, view_box: &RectF, rect: &RectF) -> RectF {
        self.bounds_to_viewport(view_box, rect)
    }

    /// URI link annotations, one entry per quad when the link has quad points.
    pub fn links(&self, view_box: &RectF) -> Vec<PdfLink> {
        let mut out = Vec::new();
        for (annot, _) in page_annotations(self.hayro_page().raw()) {
            if subtype(&annot) != b"Link" {
                continue;
            }
            let Some(uri) = uri_bytes(self, &annot).as_deref().and_then(decode_uri) else {
                continue;
            };
            let quads = annot
                .get::<Array<'_>>(b"QuadPoints")
                .map(|q| q.iter::<f32>().collect::<Vec<_>>())
                .unwrap_or_default();
            let quads = quad_bounds(&quads);
            if !quads.is_empty() {
                for quad in quads {
                    out.push(PdfLink {
                        rect: self.view_rect(view_box, &quad),
                        uri: uri.clone(),
                    });
                }
                continue;
            }
            if let Some(rect) = annot_rect(&annot) {
                out.push(PdfLink {
                    rect: self.view_rect(view_box, &rect),
                    uri,
                });
            }
        }
        out
    }

    /// Every annotation of the page, with dictionary metadata.
    pub fn annotations(&self, view_box: &RectF) -> Vec<PdfAnnotation> {
        page_annotations(self.hayro_page().raw())
            .into_iter()
            .map(|(annot, object_number)| {
                let kind = subtype(&annot);
                let has_attachment_points = matches!(
                    kind.as_slice(),
                    b"Link" | b"Highlight" | b"Underline" | b"Squiggly" | b"StrikeOut"
                );
                let quadpoint_rects = if has_attachment_points {
                    let quads = annot
                        .get::<Array<'_>>(b"QuadPoints")
                        .map(|q| q.iter::<f32>().collect::<Vec<_>>())
                        .unwrap_or_default();
                    quad_bounds(&quads)
                        .iter()
                        .map(|q| self.view_rect(view_box, q))
                        .collect()
                } else {
                    Vec::new()
                };
                let uri_raw = (kind == b"Link").then(|| uri_bytes(self, &annot)).flatten();
                PdfAnnotation {
                    object_number: object_number.filter(|n| *n > 0),
                    subtype: subtype_name(&kind).to_string(),
                    contents: string_if_present(&annot, b"Contents"),
                    created: string_if_present(&annot, b"CreationDate"),
                    modified: string_if_present(&annot, b"M"),
                    title: string_if_present(&annot, b"T"),
                    rect: annot_rect(&annot).map(|r| self.view_rect(view_box, &r)),
                    has_attachment_points,
                    quadpoint_rects,
                    uri: uri_raw.as_deref().and_then(decode_uri),
                    uri_raw,
                }
            })
            .collect()
    }

    /// Whether a non-hidden, non-popup annotation paints text at the top
    /// level of its appearance (backs the `AnnotationText` complexity
    /// signal, so deliberately shallow).
    pub fn has_annotation_text(&self) -> bool {
        // PDFium's flatten merges the widgets into the page content and
        // hides every other annotation first, so none is left to paint text.
        if self.flattened {
            return false;
        }
        page_annotations(self.hayro_page().raw())
            .iter()
            .any(|(annot, _)| {
                flags(annot) & FLAG_HIDDEN == 0
                    && subtype(annot) != b"Popup"
                    && appearance_paints_text(annot, false)
            })
    }

    /// Whether a visible widget paints text through its appearance (never
    /// after a flatten, which merged them into the content).
    pub fn has_form_widget_text(&self) -> bool {
        !self.flattened && !text_widgets(self.hayro_page().raw()).is_empty()
    }

    /// Viewport rects of the visible widgets that paint text.
    pub fn form_widget_text_rects(&self, view_box: &RectF) -> Vec<RectF> {
        if self.flattened {
            return Vec::new();
        }
        text_widgets(self.hayro_page().raw())
            .iter()
            .filter_map(annot_rect)
            .map(|rect| self.view_rect(view_box, &rect))
            .collect()
    }

    /// Mark the page's widget appearances as page content (see
    /// [`super::Document::flatten_form_widgets`]).
    pub fn flatten_form_widgets_for_display(&self) -> bool {
        self.document()
            .flatten_form_widgets(self.page_index() as i32)
            .ok()
            .flatten()
            .is_some()
    }

    /// AcroForm widgets of the page with their field metadata.
    pub fn form_fields(
        &self,
        _form: &FormEnvironment<'_, '_>,
        view_box: &RectF,
        page_number: u32,
    ) -> Vec<PdfFormField> {
        let mut out = Vec::new();
        let mut widget_index = 0;
        for (annotation_index, (annot, object_number)) in page_annotations(self.hayro_page().raw())
            .into_iter()
            .enumerate()
        {
            if subtype(&annot) != b"Widget" {
                continue;
            }
            let field = field_of(&annot);
            let parent = annot.get::<Dict<'_>>(b"Parent");
            let field_type = inherited(&field, b"FT", |d, k| {
                d.get::<Name<'_>>(k).map(|n| n.as_ref().to_vec())
            });
            let field_flags = inherited(&field, b"Ff", |d, k| d.get::<i32>(k)).unwrap_or(0);
            let kind = match field_type.as_deref() {
                Some(b"Btn") if field_flags & FF_PUSHBUTTON != 0 => "pushbutton",
                Some(b"Btn") if field_flags & FF_RADIO != 0 => "radio",
                Some(b"Btn") => "checkbox",
                Some(b"Tx") => "text",
                Some(b"Ch") if field_flags & FF_COMBO != 0 => "combobox",
                Some(b"Ch") => "listbox",
                Some(b"Sig") => "signature",
                _ => "unknown",
            };
            let name = first_present([
                full_name(&field),
                dict_text(&annot, b"T"),
                parent.as_ref().and_then(|p| dict_text(p, b"T")),
            ]);
            let alternate_name = first_present([
                inherited(&field, b"TU", |d, k| dict_text(d, k)),
                dict_text(&annot, b"TU"),
                parent.as_ref().and_then(|p| dict_text(p, b"TU")),
            ]);
            let value = first_present([
                inherited(&field, b"V", |d, k| dict_text_or_name(d, k)),
                dict_text_or_name(&annot, b"V"),
                parent.as_ref().and_then(|p| dict_text_or_name(p, b"V")),
            ]);
            let appearance_state = annot.get::<Name<'_>>(b"AS").map(|n| n.as_str().to_string());
            let checkable = matches!(kind, "checkbox" | "radio");
            let mut export_value = first_present([
                checkable.then(|| on_state(&annot)).flatten(),
                dict_text_or_name(&annot, b"V"),
            ]);
            if export_value.is_none() {
                export_value = appearance_state.clone().filter(|state| state != "Off");
            }
            let checked = checkable.then(|| {
                appearance_state
                    .as_deref()
                    .is_some_and(|state| state != "Off")
                    || match (&value, on_state(&annot)) {
                        (Some(v), Some(on)) => *v == on,
                        _ => false,
                    }
            });
            let siblings: Vec<Dict<'_>> = if checkable {
                field
                    .get::<Array<'_>>(b"Kids")
                    .map(|kids| kids.iter::<Dict<'_>>().collect())
                    .unwrap_or_else(|| vec![annot.clone()])
            } else {
                Vec::new()
            };
            let control_count = checkable.then_some(siblings.len().max(1) as i32);
            let control_index = checkable.then(|| {
                siblings
                    .iter()
                    .position(|kid| kid.data() == annot.data())
                    .unwrap_or(0) as i32
            });
            let (options, selected_options) = if kind == "combobox" || kind == "listbox" {
                choice_options(&field, value.as_deref())
            } else {
                (Vec::new(), Vec::new())
            };
            let id = name
                .clone()
                .or_else(|| object_number.map(|n| format!("pdf-object-{n}")))
                .unwrap_or_else(|| format!("page-{page_number}-widget-{widget_index}"));
            out.push(PdfFormField {
                id,
                field_type: kind.to_string(),
                page: page_number,
                annotation_index: annotation_index as i32,
                widget_index,
                object_number,
                name,
                alternate_name,
                value,
                export_value,
                field_flags,
                control_count,
                control_index,
                checked,
                rect: annot_rect(&annot).map(|r| self.view_rect(view_box, &r)),
                options,
                selected_options,
            });
            widget_index += 1;
        }
        out
    }
}

/// `(option labels, selected labels)` of a choice field. `/Opt` entries are
/// labels or `[export, label]` pairs; selection comes from `/I` indices or
/// from `/V` matching a label or export value.
fn choice_options(field: &Dict<'_>, value: Option<&str>) -> (Vec<String>, Vec<String>) {
    let entries: Vec<(String, String)> = inherited(field, b"Opt", |d, k| d.get::<Array<'_>>(k))
        .map(|opt| {
            opt.iter::<Object<'_>>()
                .filter_map(|entry| match entry {
                    Object::String(s) => {
                        let label = decode_text_string(s.as_bytes());
                        Some((label.clone(), label))
                    }
                    Object::Array(pair) => {
                        let items: Vec<hayro::hayro_syntax::object::String<'_>> =
                            pair.iter().collect();
                        let export = items.first().map(|s| decode_text_string(s.as_bytes()))?;
                        let label = items
                            .get(1)
                            .map(|s| decode_text_string(s.as_bytes()))
                            .unwrap_or_else(|| export.clone());
                        Some((export, label))
                    }
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    let selected_indices: Vec<usize> = inherited(field, b"I", |d, k| d.get::<Array<'_>>(k))
        .map(|indices| {
            indices
                .iter::<i32>()
                .filter_map(|i| usize::try_from(i).ok())
                .collect()
        })
        .unwrap_or_default();
    let values: Vec<String> = match inherited(field, b"V", |d, k| d.get::<Object<'_>>(k)) {
        Some(Object::Array(array)) => array
            .iter::<hayro::hayro_syntax::object::String<'_>>()
            .map(|s| decode_text_string(s.as_bytes()))
            .collect(),
        _ => value.map(|v| vec![v.to_string()]).unwrap_or_default(),
    };
    let selected = entries
        .iter()
        .enumerate()
        .filter(|(i, (export, label))| {
            selected_indices.contains(i) || values.iter().any(|v| v == export || v == label)
        })
        .map(|(_, (_, label))| label.clone())
        .collect();
    (
        entries.into_iter().map(|(_, label)| label).collect(),
        selected,
    )
}
