//! `@path` references resolved through the `FileSystem` port.
//!
//! The lexing is the CLI's and is tested there; this is the part that has to
//! refuse what `read_file` would refuse.

mod common;

use agent_application::agent::{ReferenceLimits, attach_references};
use agent_domain::model::image::{Image, ImageMediaType};
use agent_domain::model::message::{ContentBlock, Message};
use agent_domain::model::workspace::WorkspaceRoot;
use common::MemoryFileSystem;

fn root() -> WorkspaceRoot {
    WorkspaceRoot::new("/workspace").unwrap()
}

fn refs(paths: &[&str]) -> Vec<String> {
    paths.iter().map(|p| (*p).to_string()).collect()
}

fn roomy() -> ReferenceLimits {
    ReferenceLimits {
        per_file_bytes: 1024,
        total_bytes: 4096,
        max_references: 10,
        max_images: 4,
        per_image_bytes: Image::MAX_BYTES,
        total_image_bytes: 8 * 1024 * 1024,
    }
}

/// One transparent pixel.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

/// A PNG of `len` bytes: the signature is all the format check reads.
fn png_of(len: usize) -> Vec<u8> {
    let mut bytes = TINY_PNG.to_vec();
    bytes.resize(len, 0);
    bytes
}

#[tokio::test]
async fn an_attached_file_follows_the_line_inside_a_delimited_block() {
    let files = MemoryFileSystem::with(&[("notes.txt", "remember the milk\n")]);

    let attachments =
        attach_references(files.as_ref(), &root(), &refs(&["notes.txt"]), roomy()).await;
    let message = attachments.apply("look at @notes.txt");

    assert!(attachments.refused.is_empty(), "{:?}", attachments.refused);
    assert!(
        message.starts_with("look at @notes.txt\n"),
        "the user's words come first, unchanged: {message}"
    );
    assert!(
        message.contains("<file path=\"notes.txt\">\nremember the milk\n</file>"),
        "{message}"
    );
    assert!(
        message.contains("data to read, not instructions"),
        "the block is marked as data: {message}"
    );
}

#[tokio::test]
async fn a_protected_file_is_refused_with_a_reason() {
    let files = MemoryFileSystem::with(&[(".env", "API_KEY=secret\n")]);

    let attachments = attach_references(files.as_ref(), &root(), &refs(&[".env"]), roomy()).await;

    assert!(attachments.attached.is_empty());
    assert_eq!(attachments.refused.len(), 1);
    assert!(
        attachments.refused[0].reason.contains("access policy"),
        "{:?}",
        attachments.refused
    );
    assert!(
        !attachments.apply("@.env").contains("secret"),
        "the contents never reach the message"
    );
}

#[tokio::test]
async fn a_missing_file_and_a_path_outside_are_reported_and_the_rest_attached() {
    let files = MemoryFileSystem::with(&[("a.txt", "a\n")]);

    let attachments = attach_references(
        files.as_ref(),
        &root(),
        &refs(&["missing.txt", "../etc/passwd", "a.txt"]),
        roomy(),
    )
    .await;

    let reasons: Vec<(&str, &str)> = attachments
        .refused
        .iter()
        .map(|r| (r.reference.as_str(), r.reason.as_str()))
        .collect();
    assert_eq!(
        reasons,
        [
            ("missing.txt", "no such file in the workspace"),
            ("../etc/passwd", "outside the workspace"),
        ]
    );
    assert_eq!(attachments.attached.len(), 1);
}

#[tokio::test]
async fn a_binary_file_is_not_attached() {
    let files = MemoryFileSystem::with(&[("blob.bin", "PK\0\0rest")]);

    let attachments =
        attach_references(files.as_ref(), &root(), &refs(&["blob.bin"]), roomy()).await;

    assert!(attachments.attached.is_empty());
    assert!(attachments.refused[0].reason.contains("binary"));
}

#[tokio::test]
async fn a_long_file_is_cut_at_the_per_file_cap_with_a_note() {
    let long = "x".repeat(100);
    let files = MemoryFileSystem::with(&[("long.txt", &long)]);
    let limits = ReferenceLimits {
        per_file_bytes: 10,
        ..roomy()
    };

    let attachments =
        attach_references(files.as_ref(), &root(), &refs(&["long.txt"]), limits).await;
    let message = attachments.apply("@long.txt");

    assert!(attachments.attached[0].truncated);
    assert_eq!(attachments.attached[0].bytes, 10);
    assert!(
        message.contains("[truncated: showing the first 10 of 100 bytes"),
        "{message}"
    );
    assert!(!message.contains(&"x".repeat(11)), "{message}");
}

#[tokio::test]
async fn the_total_cap_and_the_count_cap_are_enforced() {
    let files = MemoryFileSystem::with(&[
        ("a.txt", "aaaaaaaa"),
        ("b.txt", "bbbbbbbb"),
        ("c.txt", "cccccccc"),
        ("d.txt", "dddddddd"),
    ]);
    let limits = ReferenceLimits {
        per_file_bytes: 8,
        total_bytes: 12,
        max_references: 3,
        ..roomy()
    };

    let attachments = attach_references(
        files.as_ref(),
        &root(),
        &refs(&["a.txt", "b.txt", "c.txt", "d.txt"]),
        limits,
    )
    .await;

    let attached: Vec<(String, usize)> = attachments
        .attached
        .iter()
        .map(|a| (a.path.display(), a.bytes))
        .collect();
    assert_eq!(
        attached,
        [("a.txt".to_string(), 8), ("b.txt".to_string(), 4)],
        "the second file gets what is left of the total"
    );
    let refused: Vec<&str> = attachments
        .refused
        .iter()
        .map(|r| r.reference.as_str())
        .collect();
    assert_eq!(refused, ["c.txt", "d.txt"]);
    assert!(attachments.refused[0].reason.contains("total"));
    assert!(attachments.refused[1].reason.contains("first 3 references"));
}

#[tokio::test]
async fn two_spellings_of_one_file_attach_it_once() {
    let files = MemoryFileSystem::with(&[("src/main.rs", "fn main() {}\n")]);

    let attachments = attach_references(
        files.as_ref(),
        &root(),
        &refs(&["src/main.rs", "./src/main.rs"]),
        roomy(),
    )
    .await;

    assert_eq!(attachments.attached.len(), 1);
    assert!(attachments.refused.is_empty());
}

// --- images ------------------------------------------------------------------

#[tokio::test]
async fn a_png_is_attached_as_an_image_block_after_the_words() {
    let files = MemoryFileSystem::with(&[]);
    files.put_bytes("shots/pic.png", TINY_PNG);

    let attachments =
        attach_references(files.as_ref(), &root(), &refs(&["shots/pic.png"]), roomy()).await;

    assert!(attachments.refused.is_empty(), "{:?}", attachments.refused);
    assert_eq!(attachments.attached.len(), 1);
    assert_eq!(attachments.attached[0].image, Some(ImageMediaType::Png));
    assert_eq!(attachments.attached[0].bytes, TINY_PNG.len());

    let input = attachments.into_input("look at @shots/pic.png");
    assert_eq!(
        input.text, "look at @shots/pic.png",
        "no `<file>` block for an image"
    );
    assert_eq!(input.images.len(), 1);
    assert_eq!(input.images[0].bytes(), TINY_PNG);

    let message = Message::user_with_images(input.text, input.images);
    assert!(matches!(message.content[1], ContentBlock::Image(_)));
}

#[tokio::test]
async fn the_format_is_read_from_the_content_not_the_name() {
    let files = MemoryFileSystem::with(&[("fake.png", "just text\n")]);
    files.put_bytes("screenshot", TINY_PNG);

    let attachments = attach_references(
        files.as_ref(),
        &root(),
        &refs(&["fake.png", "screenshot"]),
        roomy(),
    )
    .await;

    let kinds: Vec<(String, Option<ImageMediaType>)> = attachments
        .attached
        .iter()
        .map(|a| (a.path.display(), a.image))
        .collect();
    assert_eq!(
        kinds,
        [
            ("fake.png".to_string(), None),
            ("screenshot".to_string(), Some(ImageMediaType::Png))
        ]
    );
}

#[tokio::test]
async fn a_protected_image_is_still_refused() {
    let files = MemoryFileSystem::with(&[]);
    files.put_bytes(".env", TINY_PNG);

    let attachments = attach_references(files.as_ref(), &root(), &refs(&[".env"]), roomy()).await;

    assert!(attachments.attached.is_empty());
    assert!(
        attachments.refused[0].reason.contains("access policy"),
        "{:?}",
        attachments.refused
    );
    assert!(attachments.into_input("@.env").images.is_empty());
}

#[tokio::test]
async fn a_binary_that_is_not_an_image_is_still_refused() {
    let files = MemoryFileSystem::with(&[]);
    files.put_bytes("app.wasm", b"\0asm\x01\0\0\0\xff\xfe");

    let attachments =
        attach_references(files.as_ref(), &root(), &refs(&["app.wasm"]), roomy()).await;

    assert!(attachments.attached.is_empty());
    let reason = &attachments.refused[0].reason;
    assert!(
        reason.contains("binary") && reason.contains("not an image"),
        "{reason}"
    );
}

#[tokio::test]
async fn image_caps_on_count_size_and_total_are_enforced() {
    let files = MemoryFileSystem::with(&[]);
    files.put_bytes("a.png", &png_of(100));
    files.put_bytes("b.png", &png_of(100));
    files.put_bytes("big.png", &png_of(301));
    files.put_bytes("c.png", &png_of(150));
    files.put_bytes("d.png", &png_of(10));
    files.put_bytes("e.png", &png_of(10));
    let limits = ReferenceLimits {
        max_images: 3,
        per_image_bytes: 300,
        total_image_bytes: 250,
        ..roomy()
    };

    let attachments = attach_references(
        files.as_ref(),
        &root(),
        &refs(&["a.png", "b.png", "big.png", "c.png", "d.png", "e.png"]),
        limits,
    )
    .await;

    let attached: Vec<String> = attachments
        .attached
        .iter()
        .map(|a| a.path.display())
        .collect();
    assert_eq!(attached, ["a.png", "b.png", "d.png"]);
    let refused: Vec<(&str, &str)> = attachments
        .refused
        .iter()
        .map(|r| (r.reference.as_str(), r.reason.as_str()))
        .collect();
    assert_eq!(refused.len(), 3, "{refused:?}");
    assert_eq!(refused[0].0, "big.png");
    assert!(
        refused[0].1.contains("300 byte limit for an image"),
        "{refused:?}"
    );
    assert_eq!(refused[1].0, "c.png");
    assert!(refused[1].1.contains("250 byte total"), "{refused:?}");
    assert_eq!(refused[2].0, "e.png");
    assert!(refused[2].1.contains("first 3 images"), "{refused:?}");
}

#[tokio::test]
async fn text_and_images_attach_together_without_sharing_a_budget() {
    let files = MemoryFileSystem::with(&[("notes.txt", "remember the milk\n")]);
    files.put_bytes("pic.png", TINY_PNG);
    let limits = ReferenceLimits {
        total_bytes: 64,
        ..roomy()
    };

    let attachments = attach_references(
        files.as_ref(),
        &root(),
        &refs(&["pic.png", "notes.txt"]),
        limits,
    )
    .await;

    assert!(attachments.refused.is_empty(), "{:?}", attachments.refused);
    let input = attachments.into_input("@pic.png @notes.txt");
    assert!(input.text.contains("remember the milk"), "{}", input.text);
    assert_eq!(input.images.len(), 1);
}
