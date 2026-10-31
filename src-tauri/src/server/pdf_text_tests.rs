use super::*;
use lopdf::{dictionary, Document, Object, Stream, StringFormat};

pub(crate) fn test_pdf(texts: &[&str]) -> Vec<u8> {
    let mut document = Document::with_version("1.5");
    let pages_id = document.new_object_id();
    let font = document.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
    });
    let resources = document.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font },
    });
    let mut pages = Vec::<Object>::new();
    for text in texts {
        let content = if text.is_empty() {
            Vec::new()
        } else {
            format!("BT /F1 18 Tf 40 220 Td ({text}) Tj ET").into_bytes()
        };
        let stream = document.add_object(Stream::new(dictionary! {}, content));
        let page = document.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => stream,
            "Resources" => resources, "MediaBox" => vec![0.into(), 0.into(), 400.into(), 300.into()],
        });
        pages.push(page.into());
    }
    document.objects.insert(
        pages_id,
        dictionary! {
            "Type" => "Pages", "Kids" => pages, "Count" => texts.len() as i64,
        }
        .into(),
    );
    let catalog = document.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    document.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).expect("创建合成 PDF");
    bytes
}

#[test]
fn 原生文本逐页提取且不编造目录() {
    let bytes = test_pdf(&["First physical page", "Second physical page"]);
    let result = extract_inner(&bytes, None, false).unwrap();
    let pages = result.metadata["page_texts"].as_array().unwrap();
    assert_eq!(pages.len(), 2);
    assert!(pages[0].as_str().unwrap().contains("First physical page"));
    assert!(pages[1].as_str().unwrap().contains("Second physical page"));
    assert_eq!(result.metadata["outline"], json!([]));
    assert_eq!(result.metadata["text_status"], "embedded");
}

#[test]
fn 混合文档保留中间空白页且不移动页码() {
    let result = extract_inner(&test_pdf(&["Page one", "", "Page three"]), None, false).unwrap();
    let pages = result.metadata["page_texts"].as_array().unwrap();
    assert_eq!(pages.len(), 3);
    assert_eq!(pages[1], "");
    assert!(pages[2].as_str().unwrap().contains("Page three"));
    assert_eq!(result.metadata["text_status"], "partial");
}

#[test]
fn 无文字文档仍保留页面图片与真实识别状态() {
    let directory = tempfile::tempdir().unwrap();
    let result = extract_inner(&test_pdf(&["", ""]), Some(directory.path()), false).unwrap();
    assert!(result.text.trim().is_empty());
    assert_eq!(result.metadata["text_status"], "unavailable");
    assert_eq!(result.metadata["ocr_page_count"], 0);
    assert_eq!(result.metadata["media"]["page-0002.jpg"], 2);
    assert_eq!(result.metadata["page_texts"].as_array().unwrap().len(), 2);
    let image = image::open(directory.path().join("page-0001.jpg")).unwrap();
    assert_eq!(image.width(), 1200);
    assert!(!result.metadata["ocr_warning"].as_str().unwrap().is_empty());
}

#[test]
fn 图片写入失败不会丢失可读文本() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("blocked");
    std::fs::write(&file, b"not a directory").unwrap();
    let result = extract_inner(&test_pdf(&["Still readable"]), Some(&file), false).unwrap();
    assert!(result.text.contains("Still readable"));
    assert!(result.metadata["media"].as_object().unwrap().is_empty());
    assert!(!result.metadata["media_warning"]
        .as_str()
        .unwrap()
        .is_empty());
}

#[test]
fn 损坏或没有页面的文件不能冒充成功导入() {
    assert!(extract_inner(b"not a PDF", None, false).is_err());
    assert!(extract_inner(&test_pdf(&[]), None, false).is_err());
}

#[test]
fn 原生书签保留重复标题并跳过非法页码与循环() {
    let mut document = Document::load_mem(&test_pdf(&["one", "two"])).unwrap();
    let pages = document.get_pages();
    let root = document.new_object_id();
    let first = document.new_object_id();
    let second = document.new_object_id();
    let child = document.new_object_id();
    let title = Object::String(
        vec![0xfe, 0xff, 0x7b, 0x2c, 0x4e, 0x00, 0x7a, 0xe0],
        StringFormat::Hexadecimal,
    );
    document
        .objects
        .insert(root, dictionary! { "First" => first }.into());
    document.objects.insert(
        first,
        dictionary! {
            "Title" => title.clone(), "Dest" => vec![pages[&1].into(), "Fit".into()],
            "Next" => second, "First" => child,
        }
        .into(),
    );
    document.objects.insert(
        second,
        dictionary! {
            "Title" => title, "Dest" => vec![pages[&2].into(), "Fit".into()], "Next" => first,
        }
        .into(),
    );
    document.objects.insert(child, dictionary! {
        "Title" => Object::string_literal("invalid"), "Dest" => vec![Object::Reference((99999, 0)), "Fit".into()],
    }.into());
    document.catalog_mut().unwrap().set("Outlines", root);
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).unwrap();
    let (outline, _) = pdf_structure(&bytes, 2);
    assert_eq!(outline.len(), 2);
    assert_eq!(outline[0]["title"], "第一章");
    assert_eq!(outline[1]["title"], "第一章");
    assert_eq!(outline[0]["locator"], 1);
    assert_eq!(outline[1]["locator"], 2);
    assert_eq!(outline[0]["synthesised"], false);
}

#[test]
fn 命名目标书签和文档标题可正常解析() {
    let mut document = Document::load_mem(&test_pdf(&["page"])).unwrap();
    let page = document.get_pages()[&1];
    let root = document.new_object_id();
    let first = document.add_object(dictionary! {
        "Title" => Object::string_literal("Named chapter"),
        "A" => dictionary! { "S" => "GoTo", "D" => Object::string_literal("chapter") },
    });
    document
        .objects
        .insert(root, dictionary! { "First" => first }.into());
    let names = dictionary! { "Dests" => dictionary! { "Names" => vec![
        Object::string_literal("chapter"), vec![page.into(), "Fit".into()].into(),
    ] } };
    document.catalog_mut().unwrap().set("Outlines", root);
    document.catalog_mut().unwrap().set("Names", names);
    let info =
        document.add_object(dictionary! { "Title" => Object::string_literal("Document title") });
    document.trailer.set("Info", info);
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).unwrap();
    let (outline, title) = pdf_structure(&bytes, 1);
    assert_eq!(title, "Document title");
    assert_eq!(outline.len(), 1);
    assert_eq!(outline[0]["locator"], 1);
}

#[test]
fn 页面图片写入遵守容量预算() {
    let directory = tempfile::tempdir().unwrap();
    assert!(save_page_image(directory.path(), "large.jpg", 2, 2, &[255; 16], 1).is_err());
    assert!(!directory.path().join("large.jpg").exists());
}
