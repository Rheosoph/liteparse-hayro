use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use hayro::hayro_interpret::InterpreterCache;
use hayro::hayro_syntax::Pdf;
use hayro::hayro_syntax::object::{Array, Dict, Name, Object, ObjectIdentifier};
use hayro::hayro_syntax::page::Page as HayroPage;
use hayro::kurbo::Rect;

use super::content::{self, ContentOptions, PageContent};
use super::error::PdfiumError;
use super::font::FontCatalog;
use super::library::Library;
use super::objects::{catalog, decode_text_string, dict_text, trailer_ref_dict};
use super::page::Page;
use super::struct_tree::StructIndex;

/// How many pages' recorded content to keep. Extraction walks pages in
/// order and queries each page several times, so a small window suffices.
const CONTENT_CACHE_PAGES: usize = 3;

/// An open PDF document.
pub struct Document<'lib> {
    /// hayro's parsed fonts and decoded objects, shared by every content
    /// pass so each font program is parsed once per document (as PDFium's
    /// page data does), not once per page. Declared before `pdf`, whose
    /// data the cached fonts borrow, so it is dropped first; see
    /// [`Document::interpreter_cache`].
    interpreter_cache: InterpreterCache<'static>,
    pdf: Pdf,
    fonts: FontCatalog,
    content_cache: RefCell<Vec<(usize, ContentOptions, Rc<PageContent>)>>,
    /// `/Rotate` and `/CropBox` overrides. PDFium writes these into the open
    /// document's page dictionaries; hayro documents are immutable, so the
    /// overrides live here and the page geometry consults them.
    rotations: RefCell<HashMap<usize, i32>>,
    crops: RefCell<HashMap<usize, Rect>>,
    /// Pages whose widget appearances were flattened into their content.
    flattened: RefCell<HashSet<usize>>,
    page_ids: RefCell<Option<HashMap<ObjectIdentifier, usize>>>,
    struct_index: RefCell<Option<Rc<StructIndex>>>,
    _lib: std::marker::PhantomData<&'lib Library>,
}

/// Read-only form access. There is no form-fill engine in the hayro
/// backend, so this only records that the document has a form catalog.
pub struct FormEnvironment<'doc, 'lib: 'doc> {
    _doc: std::marker::PhantomData<&'doc Document<'lib>>,
}

/// One entry in the document's outline (bookmarks tree).
#[derive(Debug, Clone)]
pub struct OutlineEntry {
    /// Hierarchy depth, 1-based (top-level entries are level 1).
    pub level: u8,
    pub title: String,
    /// Zero-based page index of the destination, or `None` if the
    /// destination isn't a page in this document.
    pub page_index: Option<i32>,
    /// Y coordinate of an `/XYZ` destination in PDF user space, or `None`.
    pub y: Option<f32>,
}

/// One raw packet from an XFA form document's `/XFA` array.
#[derive(Debug, Clone)]
pub struct XfaPacket {
    pub index: i32,
    pub name: Option<String>,
    pub content: Option<Vec<u8>>,
}

/// Signature summary used for document provenance metadata.
#[derive(Debug, Clone, Copy, Default)]
pub struct SignatureSummary {
    pub count: Option<u32>,
    pub byte_range_reaches_eof: Option<bool>,
}

impl<'lib> Document<'lib> {
    pub(crate) fn open(bytes: Vec<u8>, password: Option<&str>) -> Result<Self, PdfiumError> {
        let pdf = match password.filter(|p| !p.is_empty()) {
            Some(password) => Pdf::new_with_password(bytes, password)?,
            None => Pdf::new(bytes)?,
        };
        Ok(Self {
            interpreter_cache: InterpreterCache::new(),
            pdf,
            fonts: FontCatalog::default(),
            content_cache: RefCell::new(Vec::new()),
            rotations: RefCell::new(HashMap::new()),
            crops: RefCell::new(HashMap::new()),
            flattened: RefCell::new(HashSet::new()),
            page_ids: RefCell::new(None),
            struct_index: RefCell::new(None),
            _lib: std::marker::PhantomData,
        })
    }

    /// The document's interpreter cache, for interpreting pages borrowed
    /// from `self`.
    fn interpreter_cache<'d>(&'d self) -> &'d InterpreterCache<'d> {
        let cache: *const InterpreterCache<'static> = &self.interpreter_cache;
        // SAFETY: the cache only ever holds fonts and objects resolved from
        // `self.pdf` (every interpretation that uses it runs on this
        // document's pages and resources), and those borrow data that
        // `self.pdf` keeps behind `Arc`s: valid wherever `self` is, moves
        // included, and longer than the cache, which drops first. The
        // `'static` is never observed; callers only see the lifetime of
        // their borrow of `self`.
        unsafe { &*cache.cast::<InterpreterCache<'d>>() }
    }

    /// The structure tree, read on first use.
    pub(crate) fn struct_index(&self) -> Rc<StructIndex> {
        Rc::clone(
            self.struct_index
                .borrow_mut()
                .get_or_insert_with(|| Rc::new(StructIndex::load(&self.pdf))),
        )
    }

    pub(crate) fn pdf(&self) -> &Pdf {
        &self.pdf
    }

    pub(crate) fn fonts(&self) -> &FontCatalog {
        &self.fonts
    }

    pub fn page_count(&self) -> i32 {
        self.pdf.pages().len() as i32
    }

    /// PDFium `FPDF_GetFormType`: 0 none, 1 AcroForm, 2 XFA full,
    /// 3 XFA foreground.
    pub fn form_type(&self) -> i32 {
        let Some(root) = catalog(&self.pdf) else {
            return 0;
        };
        let Some(acro_form) = root.get::<Dict<'_>>(b"AcroForm") else {
            return 0;
        };
        if !acro_form.contains_key(b"XFA") {
            return 1;
        }
        if root.get::<bool>(b"NeedsRendering").unwrap_or(false) {
            2
        } else {
            3
        }
    }

    /// Whether the catalog declares the document tagged (`/MarkInfo /Marked`).
    pub fn is_tagged(&self) -> bool {
        catalog(&self.pdf)
            .and_then(|root| root.get::<Dict<'_>>(b"MarkInfo"))
            .and_then(|info| info.get::<bool>(b"Marked"))
            .unwrap_or(false)
    }

    pub fn form_environment(&self) -> Option<FormEnvironment<'_, 'lib>> {
        (self.form_type() != 0).then_some(FormEnvironment {
            _doc: std::marker::PhantomData,
        })
    }

    pub fn page(&self, index: i32) -> Result<Page<'_, 'lib>, PdfiumError> {
        let pages = self.pdf.pages();
        let index = usize::try_from(index).map_err(|_| PdfiumError::PageNotFound)?;
        let page = pages.get(index).ok_or(PdfiumError::PageNotFound)?;
        let user_unit = page
            .raw()
            .get::<f32>(b"UserUnit")
            .filter(|u| u.is_finite() && *u >= 1.0)
            .unwrap_or(1.0);
        Ok(Page {
            doc: self,
            index,
            page,
            user_unit,
            flattened: self.flattened.borrow().contains(&index),
        })
    }

    /// PDFium builds a separate one-page document holding just the widget
    /// appearances; the hayro backend flattens in place instead (see
    /// [`Self::flatten_form_widgets`]).
    pub fn widget_appearance_copy(&self, _index: i32) -> Option<Document<'lib>> {
        None
    }

    /// Treat the visible widget appearances of page `index` as page content
    /// from now on and return the page in that state. `Ok(None)` when the
    /// page has no visible widget appearance to flatten.
    pub fn flatten_form_widgets(&self, index: i32) -> Result<Option<Page<'_, 'lib>>, PdfiumError> {
        let page = self.page(index)?;
        let has_appearance = content::visible_widgets(page.hayro_page().raw())
            .iter()
            .any(|annot| content::normal_appearance(annot).is_some());
        if !has_appearance {
            return Ok(None);
        }
        self.flattened.borrow_mut().insert(page.page_index());
        self.page(index).map(Some)
    }

    /// Value of `tag` in the Info dictionary. `None` when the document has
    /// no Info dictionary; `Some("")` when the key is missing or empty.
    pub fn meta_text(&self, tag: &str) -> Option<String> {
        let info = trailer_ref_dict(&self.pdf, b"Info")?;
        Some(dict_text(&info, tag.as_bytes()).unwrap_or_default())
    }

    /// The `/PageLabels` label for a zero-based page index.
    pub fn page_label(&self, page_index: i32) -> Option<String> {
        if page_index < 0 {
            return None;
        }
        let labels = catalog(&self.pdf)?.get::<Dict<'_>>(b"PageLabels")?;
        let (start, label) = number_tree_floor(&labels, page_index, 0)?;
        let prefix = dict_text(&label, b"P").unwrap_or_default();
        let first = label.get::<i32>(b"St").unwrap_or(1);
        let number = first + (page_index - start);
        let numeral = match label.get::<Name<'_>>(b"S").as_deref() {
            Some(b"D") => number.to_string(),
            Some(b"R") => roman(number).to_uppercase(),
            Some(b"r") => roman(number),
            Some(b"A") => letters(number).to_uppercase(),
            Some(b"a") => letters(number),
            _ => String::new(),
        };
        let text = format!("{prefix}{numeral}");
        (!text.is_empty()).then_some(text)
    }

    /// Version from the file header (`%PDF-1.7` → 17).
    pub fn file_version(&self) -> Option<i32> {
        let data: &[u8] = self.pdf.data().as_ref();
        let head = &data[..data.len().min(1024)];
        let pos = memchr::memmem::find(head, b"%PDF-")?;
        let digits = head.get(pos + 5..pos + 8)?;
        let (major, minor) = (digits[0], digits[2]);
        (major.is_ascii_digit() && digits[1] == b'.' && minor.is_ascii_digit())
            .then(|| ((major - b'0') * 10 + (minor - b'0')) as i32)
    }

    /// Security-handler revision, or `-1` for an unencrypted document.
    pub fn security_handler_revision(&self) -> i32 {
        trailer_ref_dict(&self.pdf, b"Encrypt")
            .and_then(|encrypt| encrypt.get::<i32>(b"R"))
            .unwrap_or(-1)
    }

    /// Permission flags (`/P`), all set for unencrypted documents.
    pub fn permissions(&self) -> u64 {
        trailer_ref_dict(&self.pdf, b"Encrypt")
            .and_then(|encrypt| encrypt.get::<i64>(b"P"))
            .map_or(0xFFFF_FFFF, |p| (p as u32) as u64)
    }

    /// Count signature fields and check whether every signature's final
    /// byte range ends at the end of the file.
    pub fn signature_summary(&self, file_size: Option<u64>) -> SignatureSummary {
        let mut signatures = Vec::new();
        if let Some(fields) = catalog(&self.pdf)
            .and_then(|root| root.get::<Dict<'_>>(b"AcroForm"))
            .and_then(|form| form.get::<Array<'_>>(b"Fields"))
        {
            let mut visited = HashSet::new();
            for field in fields.iter::<Dict<'_>>() {
                collect_signatures(&field, None, &mut signatures, &mut visited, 0);
            }
        }
        let count = signatures.len() as u32;
        let Some(file_size) = file_size.filter(|_| count > 0) else {
            return SignatureSummary {
                count: Some(count),
                byte_range_reaches_eof: None,
            };
        };
        let mut known = false;
        let mut reaches_eof = true;
        for ranges in signatures {
            if !(2..=8).contains(&ranges.len()) {
                continue;
            }
            known = true;
            let start = ranges[ranges.len() - 2];
            let length = ranges[ranges.len() - 1];
            if start < 0
                || length < 0
                || u64::try_from(start + length).is_ok_and(|end| end < file_size)
            {
                reaches_eof = false;
            }
        }
        SignatureSummary {
            count: Some(count),
            byte_range_reaches_eof: known.then_some(reaches_eof),
        }
    }

    pub fn xfa_packet_count(&self) -> i32 {
        self.xfa_packets().len() as i32
    }

    /// Packets of the AcroForm `/XFA` entry: a single stream, or an array of
    /// `name, stream` pairs.
    pub fn xfa_packets(&self) -> Vec<XfaPacket> {
        let Some(xfa) = catalog(&self.pdf)
            .and_then(|root| root.get::<Dict<'_>>(b"AcroForm"))
            .and_then(|form| form.get::<Object<'_>>(b"XFA"))
        else {
            return Vec::new();
        };
        match xfa {
            Object::Stream(stream) => vec![XfaPacket {
                index: 0,
                name: Some(String::new()),
                content: stream.decoded().ok().map(|data| data.into_owned()),
            }],
            Object::Array(array) => {
                let items: Vec<Object<'_>> = array.iter::<Object<'_>>().collect();
                items
                    .chunks(2)
                    .enumerate()
                    .map(|(index, pair)| XfaPacket {
                        index: index as i32,
                        name: match pair.first() {
                            Some(Object::String(s)) => Some(decode_text_string(s.as_bytes())),
                            _ => None,
                        },
                        content: match pair.get(1) {
                            Some(Object::Stream(stream)) => {
                                stream.decoded().ok().map(|data| data.into_owned())
                            }
                            _ => None,
                        },
                    })
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    /// The outline (bookmarks) in pre-order.
    pub fn outline(&self) -> Vec<OutlineEntry> {
        let mut out = Vec::new();
        let Some(root) = catalog(&self.pdf) else {
            return out;
        };
        let Some(first) = root
            .get::<Dict<'_>>(b"Outlines")
            .and_then(|outlines| outlines.get::<Dict<'_>>(b"First"))
        else {
            return out;
        };
        let mut visited = HashSet::new();
        self.walk_outline(&root, first, 1, &mut out, &mut visited);
        out
    }

    fn walk_outline(
        &self,
        root: &Dict<'_>,
        first: Dict<'_>,
        level: u8,
        out: &mut Vec<OutlineEntry>,
        visited: &mut HashSet<Vec<u8>>,
    ) {
        let mut current = Some(first);
        while let Some(item) = current {
            let key = item
                .obj_id()
                .map(|id| format!("{}:{}", id.obj_number, id.gen_number).into_bytes())
                .unwrap_or_else(|| item.data().to_vec());
            if !visited.insert(key) || out.len() > 100_000 {
                return;
            }
            let title = dict_text(&item, b"Title").unwrap_or_default();
            let dest = item.get::<Object<'_>>(b"Dest").or_else(|| {
                item.get::<Dict<'_>>(b"A")
                    .filter(|action| action.get::<Name<'_>>(b"S").as_deref() == Some(&b"GoTo"[..]))
                    .and_then(|action| action.get::<Object<'_>>(b"D"))
            });
            let (page_index, y) = dest
                .and_then(|dest| self.resolve_dest(root, dest, 0))
                .unwrap_or((None, None));
            out.push(OutlineEntry {
                level,
                title,
                page_index,
                y,
            });
            if let Some(child) = item.get::<Dict<'_>>(b"First") {
                self.walk_outline(root, child, level.saturating_add(1), out, visited);
            }
            current = item.get::<Dict<'_>>(b"Next");
        }
    }

    /// `(page index, /XYZ top)` of a destination (array, name or string).
    fn resolve_dest(
        &self,
        root: &Dict<'_>,
        dest: Object<'_>,
        depth: u32,
    ) -> Option<(Option<i32>, Option<f32>)> {
        if depth > 4 {
            return None;
        }
        match dest {
            Object::Array(array) => {
                let mut items = array.raw_iter();
                let first = items.next()?;
                let page_index = match first.as_obj_ref() {
                    Some(r) => self.page_index_of(r.into()).map(|i| i as i32),
                    None => array.iter::<i32>().next(),
                };
                let kind = array.iter::<Object<'_>>().nth(1);
                let y = match kind {
                    Some(Object::Name(name)) if name.as_ref() == b"XYZ" => {
                        array.iter::<Object<'_>>().nth(3).and_then(|o| match o {
                            Object::Number(n) => Some(n.as_f32()),
                            _ => None,
                        })
                    }
                    _ => None,
                };
                Some((page_index, y))
            }
            Object::Dict(dict) => {
                // A named destination's value may be `<< /D [...] >>`.
                let inner = dict.get::<Object<'_>>(b"D")?;
                self.resolve_dest(root, inner, depth + 1)
            }
            Object::Name(name) => {
                let target = lookup_named_dest(root, name.as_ref())?;
                self.resolve_dest(root, target, depth + 1)
            }
            Object::String(name) => {
                let target = lookup_named_dest(root, name.as_bytes())?;
                self.resolve_dest(root, target, depth + 1)
            }
            _ => None,
        }
    }

    fn page_index_of(&self, id: ObjectIdentifier) -> Option<usize> {
        if self.page_ids.borrow().is_none() {
            let map = self
                .pdf
                .pages()
                .iter()
                .enumerate()
                .filter_map(|(index, page)| page.raw().obj_id().map(|id| (id, index)))
                .collect();
            *self.page_ids.borrow_mut() = Some(map);
        }
        self.page_ids.borrow().as_ref()?.get(&id).copied()
    }

    /// Whether `/ViewerPreferences /Direction` is `R2L`.
    pub(crate) fn is_direction_r2l(&self) -> bool {
        catalog(&self.pdf)
            .and_then(|root| root.get::<Dict<'_>>(b"ViewerPreferences"))
            .and_then(|prefs| prefs.get::<Name<'_>>(b"Direction"))
            .is_some_and(|dir| dir.as_ref() == b"R2L")
    }

    pub(crate) fn rotation_override(&self, index: usize) -> Option<i32> {
        self.rotations.borrow().get(&index).copied()
    }

    pub(crate) fn set_rotation_override(&self, index: usize, quarter_turns: i32) {
        self.rotations.borrow_mut().insert(index, quarter_turns);
        self.invalidate(index);
    }

    pub(crate) fn crop_override(&self, index: usize) -> Option<Rect> {
        self.crops.borrow().get(&index).copied()
    }

    pub(crate) fn set_crop_override(&self, index: usize, rect: Rect) {
        self.crops.borrow_mut().insert(index, rect);
        self.invalidate(index);
    }

    fn invalidate(&self, index: usize) {
        self.content_cache
            .borrow_mut()
            .retain(|(i, ..)| *i != index);
    }

    /// The recorded content of page `index`, from cache when possible. A
    /// request is served by any cached pass recording at least as much.
    pub(crate) fn page_content(
        &self,
        index: usize,
        page: &HayroPage<'_>,
        options: ContentOptions,
    ) -> Rc<PageContent> {
        {
            let cache = self.content_cache.borrow();
            if let Some((_, _, content)) = cache.iter().find(|(i, cached, _)| {
                *i == index
                    && cached.flatten_widgets == options.flatten_widgets
                    && (cached.image_pixels || !options.image_pixels)
                    && (cached.image_streams || !options.image_streams)
            }) {
                return Rc::clone(content);
            }
        }
        let content = Rc::new(content::collect(
            page,
            &self.fonts,
            self.interpreter_cache(),
            options,
        ));
        let mut cache = self.content_cache.borrow_mut();
        cache.insert(0, (index, options, Rc::clone(&content)));
        cache.truncate(CONTENT_CACHE_PAGES * 2);
        content
    }
}

impl FormEnvironment<'_, '_> {
    /// Document JavaScript and open actions are not executed by the hayro
    /// backend.
    pub fn run_document_actions(&self) {}
}

/// The entry of a number tree with the largest key `<= key`.
fn number_tree_floor<'a>(node: &Dict<'a>, key: i32, depth: u32) -> Option<(i32, Dict<'a>)> {
    if depth > 32 {
        return None;
    }
    if let Some(nums) = node.get::<Array<'_>>(b"Nums") {
        let entries: Vec<Object<'_>> = nums.iter::<Object<'_>>().collect();
        let mut best: Option<(i32, Dict<'a>)> = None;
        for pair in entries.chunks(2) {
            let (Some(Object::Number(n)), Some(Object::Dict(value))) = (pair.first(), pair.get(1))
            else {
                continue;
            };
            let n = n.as_i64() as i32;
            if n <= key && best.as_ref().is_none_or(|(b, _)| n >= *b) {
                best = Some((n, value.clone()));
            }
        }
        return best;
    }
    let kids = node.get::<Array<'_>>(b"Kids")?;
    let mut best: Option<(i32, Dict<'a>)> = None;
    for kid in kids.iter::<Dict<'_>>() {
        if let Some([lo, _hi]) = kid.get::<[i32; 2]>(b"Limits")
            && lo > key
        {
            continue;
        }
        if let Some(found) = number_tree_floor(&kid, key, depth + 1)
            && best.as_ref().is_none_or(|(b, _)| found.0 >= *b)
        {
            best = Some(found);
        }
    }
    best
}

/// Look `name` up in the catalog's `/Dests` dictionary or `/Names /Dests`
/// name tree.
fn lookup_named_dest<'a>(root: &Dict<'a>, name: &[u8]) -> Option<Object<'a>> {
    if let Some(dests) = root.get::<Dict<'_>>(b"Dests")
        && let Some(target) = dests.get::<Object<'_>>(name)
    {
        return Some(target);
    }
    let tree = root.get::<Dict<'_>>(b"Names")?.get::<Dict<'_>>(b"Dests")?;
    name_tree_lookup(&tree, name, 0)
}

fn name_tree_lookup<'a>(node: &Dict<'a>, name: &[u8], depth: u32) -> Option<Object<'a>> {
    if depth > 32 {
        return None;
    }
    if let Some(names) = node.get::<Array<'_>>(b"Names") {
        let entries: Vec<Object<'_>> = names.iter::<Object<'_>>().collect();
        for pair in entries.chunks(2) {
            if let (Some(Object::String(key)), Some(value)) = (pair.first(), pair.get(1))
                && key.as_bytes() == name
            {
                return Some(value.clone());
            }
        }
        return None;
    }
    for kid in node.get::<Array<'_>>(b"Kids")?.iter::<Dict<'_>>() {
        if let Some(limits) = kid.get::<Array<'_>>(b"Limits") {
            let bounds: Vec<hayro::hayro_syntax::object::String<'_>> = limits.iter().collect();
            if bounds.len() == 2 && (name < bounds[0].as_bytes() || name > bounds[1].as_bytes()) {
                continue;
            }
        }
        if let Some(found) = name_tree_lookup(&kid, name, depth + 1) {
            return Some(found);
        }
    }
    None
}

/// Signature fields (`/FT /Sig` with a signature value), with each value's
/// `/ByteRange`. Field type and value inherit down the field tree.
fn collect_signatures(
    field: &Dict<'_>,
    inherited_type: Option<&[u8]>,
    out: &mut Vec<Vec<i64>>,
    visited: &mut HashSet<Vec<u8>>,
    depth: u32,
) {
    if depth > 32 || !visited.insert(field.data().to_vec()) {
        return;
    }
    let field_type = field
        .get::<Name<'_>>(b"FT")
        .map(|n| n.as_ref().to_vec())
        .or_else(|| inherited_type.map(<[u8]>::to_vec));
    if let Some(kids) = field.get::<Array<'_>>(b"Kids") {
        let kids: Vec<Dict<'_>> = kids.iter::<Dict<'_>>().collect();
        if kids
            .iter()
            .any(|kid| kid.contains_key(b"T") || kid.contains_key(b"FT"))
        {
            for kid in &kids {
                collect_signatures(kid, field_type.as_deref(), out, visited, depth + 1);
            }
            return;
        }
    }
    if field_type.as_deref() == Some(&b"Sig"[..])
        && let Some(value) = field.get::<Dict<'_>>(b"V")
    {
        let ranges = value
            .get::<Array<'_>>(b"ByteRange")
            .map(|array| array.iter::<i64>().collect())
            .unwrap_or_default();
        out.push(ranges);
    }
}

fn roman(mut n: i32) -> String {
    if n <= 0 {
        return n.to_string();
    }
    const TABLE: [(i32, &str); 13] = [
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    let mut out = String::new();
    for (value, numeral) in TABLE {
        while n >= value {
            out.push_str(numeral);
            n -= value;
        }
    }
    out
}

/// PDF alphabetic page labels: a..z, then aa..zz, aaa..
fn letters(n: i32) -> String {
    if n <= 0 {
        return n.to_string();
    }
    let index = (n - 1) as usize;
    let letter = (b'a' + (index % 26) as u8) as char;
    std::iter::repeat_n(letter, index / 26 + 1).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roman_and_letter_labels() {
        assert_eq!(roman(4), "iv");
        assert_eq!(roman(1994), "mcmxciv");
        assert_eq!(letters(1), "a");
        assert_eq!(letters(27), "aa");
        assert_eq!(letters(28), "bb");
    }
}
