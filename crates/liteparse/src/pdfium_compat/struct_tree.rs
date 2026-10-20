//! The tagged-PDF structure tree, scoped to a page the way PDFium's
//! `FPDF_StructTree_GetForPage` scopes it: a page sees the elements that tag
//! content on it (marked-content ids or annotation references) plus their
//! ancestors, and each element only the marked content and annotation
//! references that belong to the page.

use std::collections::{BTreeMap, HashMap, HashSet};

use hayro::hayro_syntax::object::{Dict, Name, Object, ObjectIdentifier};

use super::objects::{catalog, decode_text_string, dict_name, dict_text};
use super::page::Page;
use super::types::RectF;

/// A scalar value from a structure element's `/A` attribute dictionary.
#[derive(Debug, Clone, PartialEq)]
pub enum StructureAttributeValue {
    Boolean(bool),
    Number(f32),
    String(String),
}

/// One element in the page-scoped structure tree.
#[derive(Debug, Clone)]
pub struct StructureElement {
    pub element_type: String,
    pub id: Option<String>,
    pub actual_text: Option<String>,
    pub alt_text: Option<String>,
    pub title: Option<String>,
    pub attributes: BTreeMap<String, StructureAttributeValue>,
    pub marked_content_ids: Vec<i32>,
    pub children: Vec<StructureElement>,
    /// PDF object numbers of annotations referenced by `/OBJR` kids.
    pub annotation_object_numbers: Vec<i32>,
}

/// One node of the structure tree, flattened in pre-order.
#[derive(Debug, Clone)]
pub struct StructNode {
    pub role: String,
    pub mcids: Vec<i32>,
    /// Union bbox (viewport space) of the content tagged with `mcids`.
    pub bbox: Option<RectF>,
    pub alt_text: Option<String>,
}

#[derive(Debug, Clone)]
enum Kid {
    Element(usize),
    Mcid {
        page: Option<ObjectIdentifier>,
        mcid: i32,
    },
    Objr {
        page: Option<ObjectIdentifier>,
        object: i32,
    },
}

#[derive(Debug)]
struct Element {
    role: String,
    id: Option<String>,
    actual_text: Option<String>,
    alt_text: Option<String>,
    title: Option<String>,
    attributes: BTreeMap<String, StructureAttributeValue>,
    kids: Vec<Kid>,
    parent: Option<usize>,
}

/// The whole structure tree, read once per document.
#[derive(Debug, Default)]
pub(crate) struct StructIndex {
    elements: Vec<Element>,
    roots: Vec<usize>,
    /// Elements with marked content or annotation references on each page.
    direct: HashMap<ObjectIdentifier, Vec<usize>>,
}

const MAX_ELEMENTS: usize = 2_000_000;

impl StructIndex {
    pub(crate) fn load(pdf: &hayro::hayro_syntax::Pdf) -> Self {
        let mut index = Self::default();
        let Some(root) = catalog(pdf).and_then(|c| c.get::<Dict<'_>>(b"StructTreeRoot")) else {
            return index;
        };
        let mut visited = HashSet::new();
        for kid in kids_of(&root) {
            if let Object::Dict(dict) = kid
                && let Some(element) = index.load_element(&dict, None, None, &mut visited, 0)
            {
                index.roots.push(element);
            }
        }
        index
    }

    fn load_element(
        &mut self,
        dict: &Dict<'_>,
        parent: Option<usize>,
        inherited_page: Option<ObjectIdentifier>,
        visited: &mut HashSet<Vec<u8>>,
        depth: u32,
    ) -> Option<usize> {
        if depth > 256 || self.elements.len() >= MAX_ELEMENTS {
            return None;
        }
        let identity = dict
            .obj_id()
            .map(|id| format!("{}:{}", id.obj_number, id.gen_number).into_bytes())
            .unwrap_or_else(|| dict.data().to_vec());
        if !visited.insert(identity) {
            return None;
        }
        let page = dict
            .get_ref(b"Pg")
            .map(ObjectIdentifier::from)
            .or(inherited_page);
        let index = self.elements.len();
        self.elements.push(Element {
            role: dict_name(dict, b"S").unwrap_or_default(),
            id: dict
                .get::<hayro::hayro_syntax::object::String<'_>>(b"ID")
                .map(|s| decode_text_string(s.as_bytes())),
            actual_text: dict_text(dict, b"ActualText").filter(|s| !s.is_empty()),
            alt_text: dict_text(dict, b"Alt").filter(|s| !s.is_empty()),
            title: dict_text(dict, b"T").filter(|s| !s.is_empty()),
            attributes: read_attributes(dict),
            kids: Vec::new(),
            parent,
        });
        let mut kids = Vec::new();
        let mut direct_pages: Vec<ObjectIdentifier> = Vec::new();
        for kid in kids_of(dict) {
            match kid {
                Object::Number(n) => {
                    kids.push(Kid::Mcid {
                        page,
                        mcid: n.as_i64() as i32,
                    });
                    direct_pages.extend(page);
                }
                Object::Dict(kid) => {
                    let kid_page = kid.get_ref(b"Pg").map(ObjectIdentifier::from).or(page);
                    match kid.get::<Name<'_>>(b"Type").as_deref() {
                        Some(b"MCR") => {
                            if let Some(mcid) = kid.get::<i32>(b"MCID") {
                                kids.push(Kid::Mcid {
                                    page: kid_page,
                                    mcid,
                                });
                                direct_pages.extend(kid_page);
                            }
                        }
                        Some(b"OBJR") => {
                            if let Some(object) = kid.get_ref(b"Obj") {
                                kids.push(Kid::Objr {
                                    page: kid_page,
                                    object: object.obj_number,
                                });
                                direct_pages.extend(kid_page);
                            }
                        }
                        _ => {
                            if let Some(child) =
                                self.load_element(&kid, Some(index), page, visited, depth + 1)
                            {
                                kids.push(Kid::Element(child));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        direct_pages.sort();
        direct_pages.dedup();
        for page in direct_pages {
            self.direct.entry(page).or_default().push(index);
        }
        self.elements[index].kids = kids;
        Some(index)
    }

    /// Elements touching `page`: those with content on it and their
    /// ancestors.
    fn on_page(&self, page: ObjectIdentifier) -> HashSet<usize> {
        let mut set = HashSet::new();
        for &element in self
            .direct
            .get(&page)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let mut current = Some(element);
            while let Some(index) = current {
                if !set.insert(index) {
                    break;
                }
                current = self.elements[index].parent;
            }
        }
        set
    }

    fn mcids(&self, element: usize, page: ObjectIdentifier) -> Vec<i32> {
        let mut mcids = Vec::new();
        for kid in &self.elements[element].kids {
            if let Kid::Mcid {
                page: Some(p),
                mcid,
            } = kid
                && *p == page
                && *mcid >= 0
                && !mcids.contains(mcid)
            {
                mcids.push(*mcid);
            }
        }
        mcids
    }

    fn build(
        &self,
        element: usize,
        page: ObjectIdentifier,
        on_page: &HashSet<usize>,
    ) -> StructureElement {
        let e = &self.elements[element];
        let mut children = Vec::new();
        let mut annotations = Vec::new();
        for kid in &e.kids {
            match kid {
                Kid::Element(child) if on_page.contains(child) => {
                    children.push(self.build(*child, page, on_page));
                }
                Kid::Objr {
                    page: Some(p),
                    object,
                } if *p == page => annotations.push(*object),
                _ => {}
            }
        }
        StructureElement {
            element_type: e.role.clone(),
            id: e.id.clone(),
            actual_text: e.actual_text.clone(),
            alt_text: e.alt_text.clone(),
            title: e.title.clone(),
            attributes: e.attributes.clone(),
            marked_content_ids: self.mcids(element, page),
            children,
            annotation_object_numbers: annotations,
        }
    }

    fn walk_nodes(
        &self,
        element: usize,
        page: ObjectIdentifier,
        on_page: &HashSet<usize>,
        bbox_of: &dyn Fn(&[i32]) -> Option<RectF>,
        out: &mut Vec<StructNode>,
    ) {
        let e = &self.elements[element];
        let mcids = self.mcids(element, page);
        out.push(StructNode {
            role: e.role.clone(),
            bbox: bbox_of(&mcids),
            mcids,
            alt_text: e.alt_text.clone(),
        });
        for kid in &e.kids {
            if let Kid::Element(child) = kid
                && on_page.contains(child)
            {
                self.walk_nodes(*child, page, on_page, bbox_of, out);
            }
        }
    }
}

/// `/K` as a list: a single kid or an array of kids.
fn kids_of<'a>(dict: &Dict<'a>) -> Vec<Object<'a>> {
    match dict.get::<Object<'_>>(b"K") {
        Some(Object::Array(array)) => array.iter::<Object<'_>>().collect(),
        Some(other) => vec![other],
        None => Vec::new(),
    }
}

/// Flatten `/A` (a dict, stream, or array of them with optional revision
/// numbers) into name → scalar.
fn read_attributes(dict: &Dict<'_>) -> BTreeMap<String, StructureAttributeValue> {
    let mut out = BTreeMap::new();
    let mut add = |attr: &Dict<'_>| {
        for (name, _) in attr.entries() {
            let key = name.as_str().to_string();
            let value = match attr.get::<Object<'_>>(name.as_ref()) {
                Some(Object::Boolean(b)) => StructureAttributeValue::Boolean(b),
                Some(Object::Number(n)) => StructureAttributeValue::Number(n.as_f32()),
                Some(Object::String(s)) => {
                    StructureAttributeValue::String(decode_text_string(s.as_bytes()))
                }
                Some(Object::Name(n)) => StructureAttributeValue::String(n.as_str().to_string()),
                _ => continue,
            };
            out.insert(key, value);
        }
    };
    match dict.get::<Object<'_>>(b"A") {
        Some(Object::Dict(attr)) => add(&attr),
        Some(Object::Stream(stream)) => add(stream.dict()),
        Some(Object::Array(array)) => {
            for item in array.iter::<Object<'_>>() {
                match item {
                    Object::Dict(attr) => add(&attr),
                    Object::Stream(stream) => add(stream.dict()),
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out
}

impl Page<'_, '_> {
    fn struct_index_and_id(&self) -> Option<(std::rc::Rc<StructIndex>, ObjectIdentifier)> {
        let page_id = self.hayro_page().raw().obj_id()?;
        let index = self.document().struct_index();
        (!index.roots.is_empty()).then_some((index, page_id))
    }

    /// The page's tagged-PDF structure tree; empty for untagged pages.
    pub fn structure_tree(&self) -> Vec<StructureElement> {
        let Some((index, page)) = self.struct_index_and_id() else {
            return Vec::new();
        };
        let on_page = index.on_page(page);
        index
            .roots
            .iter()
            .filter(|root| on_page.contains(root))
            .map(|&root| index.build(root, page, &on_page))
            .collect()
    }

    /// The page's structure tree flattened in pre-order, with each node's
    /// bbox from the content tagged with its marked-content ids.
    pub fn struct_tree(&self, view_box: &RectF) -> Vec<StructNode> {
        let Some((index, page)) = self.struct_index_and_id() else {
            return Vec::new();
        };
        let on_page = index.on_page(page);
        if on_page.is_empty() {
            return Vec::new();
        }
        let content = self.content(super::content::ContentOptions::default());
        let vp = self.viewport_transform(view_box);
        let bbox_of = |mcids: &[i32]| -> Option<RectF> {
            let mut acc: Option<RectF> = None;
            for mcid in mcids {
                let Some(bounds) = content.mcid_bounds.get(mcid) else {
                    continue;
                };
                let b = vp.transform_bounds(&RectF::from_kurbo_page(*bounds));
                acc = Some(match acc {
                    Some(a) => RectF {
                        left: a.left.min(b.left),
                        top: a.top.min(b.top),
                        right: a.right.max(b.right),
                        bottom: a.bottom.max(b.bottom),
                    },
                    None => b,
                });
            }
            acc
        };
        let mut out = Vec::new();
        for &root in &index.roots {
            if on_page.contains(&root) {
                index.walk_nodes(root, page, &on_page, &bbox_of, &mut out);
            }
        }
        out
    }
}
