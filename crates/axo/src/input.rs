//! Prompt parts: what apps send as a user message, how handed-over content is saved, and how
//! parts become model blocks.

use std::io::ErrorKind;
use std::path::PathBuf;

use axo_ai::UserBlock;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::context::skills::{Skill, expand_skill_command};
use crate::error::Error;
use crate::schemes::LOCAL_DIR;
use crate::tools::files::{IMAGE_SNIFF_BYTES, detect_image_mime, process_image};
use crate::tools::{Located, SessionTools};

/// One piece of a user message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PromptPart {
    /// Typed text.
    Text { text: String },
    /// Pasted text kept inline; reaches the model like `Text`, marked so apps can redraw it collapsed.
    Paste { text: String },
    /// A reference to an existing file or directory: a path (relative to cwd, absolute, `~`) or a scheme URL.
    File { path: String },
    /// Content the user hands over (a paste saved as a file, a file from outside the project, an image).
    /// Input only: axo saves it under `local://` and stores it as `Saved`.
    Content {
        name: String,
        #[serde(with = "base64_bytes")]
        bytes: Vec<u8>,
    },
    /// A file axo saved from `Content`; `image` files also reach the model as pixels.
    Saved { url: String, image: bool },
}

/// A user message as parts; plain strings (`&str`, `String`, `&String`) convert to one `Text` part.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptInput {
    pub parts: Vec<PromptPart>,
}

impl From<&str> for PromptInput {
    fn from(text: &str) -> PromptInput {
        PromptInput::from(text.to_owned())
    }
}

impl From<&String> for PromptInput {
    fn from(text: &String) -> PromptInput {
        PromptInput::from(text.clone())
    }
}

impl From<String> for PromptInput {
    fn from(text: String) -> PromptInput {
        PromptInput {
            parts: vec![PromptPart::Text { text }],
        }
    }
}

impl From<Vec<PromptPart>> for PromptInput {
    fn from(parts: Vec<PromptPart>) -> PromptInput {
        PromptInput { parts }
    }
}

mod base64_bytes {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        STANDARD.decode(text).map_err(D::Error::custom)
    }
}

/// How the model sees a path it should open itself: `@path`, quoted when it contains whitespace.
pub(crate) fn reference(path: &str) -> String {
    if path.chars().any(char::is_whitespace) {
        format!("@\"{path}\"")
    } else {
        format!("@{path}")
    }
}

/// The parts as one text: text verbatim, files and content as references.
fn flatten(parts: &[PromptPart]) -> String {
    let mut text = String::new();
    for part in parts {
        match part {
            PromptPart::Text { text: part } | PromptPart::Paste { text: part } => {
                text.push_str(part);
            }
            PromptPart::File { path } => text.push_str(&reference(path)),
            PromptPart::Saved { url, .. } => text.push_str(&reference(url)),
            PromptPart::Content { name, .. } => text.push_str(&reference(name)),
        }
    }
    text
}

/// The text to parse as a command: `None` unless the message starts with typed `/` text.
pub(crate) fn command_text(parts: &[PromptPart]) -> Option<String> {
    match parts.first() {
        Some(PromptPart::Text { text }) if text.starts_with('/') => Some(flatten(parts)),
        _ => None,
    }
}

/// Files `prepare` saved for one message; dropped unkept (rejected or failed input), they are removed.
#[must_use]
pub(crate) struct SavedFiles(Vec<PathBuf>);

impl SavedFiles {
    /// The message was recorded: the files stay.
    pub(crate) fn keep(mut self) {
        self.0.clear();
    }
}

impl Drop for SavedFiles {
    fn drop(&mut self) {
        // ponytail: blocking unlink in Drop; failure path only, one file per attachment
        for path in &self.0 {
            if let Err(error) = std::fs::remove_file(path) {
                tracing::warn!(path = %path.display(), %error, "removing unsubmitted content failed");
            }
        }
    }
}

/// Prepared parts and the files saved for them.
pub(crate) struct Prepared {
    pub(crate) parts: Vec<PromptPart>,
    pub(crate) saved: SavedFiles,
}

/// Validates file references and saves handed-over content under `local://`; runs before
/// anything is written to the log; on error nothing it saved remains. Returns the parts to
/// store and the files saved for them.
pub(crate) async fn prepare(
    parts: Vec<PromptPart>,
    tools: &SessionTools,
) -> Result<Prepared, Error> {
    let mut prepared = Vec::with_capacity(parts.len());
    let mut saved = SavedFiles(Vec::new());
    for part in parts {
        let part = match part {
            PromptPart::File { path } => {
                let target = match tools
                    .locate(&path)
                    .await
                    .map_err(|error| Error::InvalidInput(error.0))?
                {
                    Located::Plain => Some(tools.resolve_read_path(&path)),
                    Located::File { path, .. } => Some(path),
                    Located::Virtual { .. } => None,
                };
                if let Some(target) = target
                    && tokio::fs::metadata(&target).await.is_err()
                {
                    return Err(Error::InvalidInput(format!("File not found: {path}")));
                }
                PromptPart::File { path }
            }
            PromptPart::Content { name, bytes } => save(tools, &name, &bytes, &mut saved).await?,
            PromptPart::Saved { url, .. } => {
                let image = saved_image(&url, tools).await?;
                PromptPart::Saved { url, image }
            }
            part @ (PromptPart::Text { .. } | PromptPart::Paste { .. }) => part,
        };
        prepared.push(part);
    }
    Ok(Prepared {
        parts: prepared,
        saved,
    })
}

/// Whether the existing `local://` file `url` sniffs as a supported image; any other URL is
/// `File not found`.
async fn saved_image(url: &str, tools: &SessionTools) -> Result<bool, Error> {
    let not_found = || Error::InvalidInput(format!("File not found: {url}"));
    if tools
        .schemes
        .exact(url)
        .is_none_or(|url| url.scheme != "local")
    {
        return Err(not_found());
    }
    let Ok(Located::File { path, .. }) = tools.locate(url).await else {
        return Err(not_found());
    };
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| not_found())?;
    let mut head = Vec::with_capacity(IMAGE_SNIFF_BYTES);
    file.take(IMAGE_SNIFF_BYTES as u64)
        .read_to_end(&mut head)
        .await
        .map_err(|source| Error::Io { path, source })?;
    Ok(detect_image_mime(&head).is_some())
}

/// Writes `bytes` to a new file in the session's `local://` root, never overwriting:
/// `name`, then `stem-2.ext`, `stem-3.ext`, … The new file joins `saved` as soon as it
/// exists, so a failed write removes it too.
async fn save(
    tools: &SessionTools,
    name: &str,
    bytes: &[u8],
    saved: &mut SavedFiles,
) -> Result<PromptPart, Error> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
        return Err(Error::InvalidInput(format!("Invalid content name: {name}")));
    }
    let dir = tools.schemes.context().session_dir.join(LOCAL_DIR);
    let io = |path: &std::path::Path| {
        let path = path.to_path_buf();
        move |source: std::io::Error| Error::Io { path, source }
    };
    tokio::fs::create_dir_all(&dir).await.map_err(io(&dir))?;
    let mut copy = 1;
    let (final_name, path, mut file) = loop {
        let candidate = numbered(name, copy);
        let path = dir.join(&candidate);
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            Ok(file) => break (candidate, path, file),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => copy += 1,
            Err(source) => return Err(Error::Io { path, source }),
        }
    };
    saved.0.push(path.clone());
    file.write_all(bytes).await.map_err(io(&path))?;
    file.sync_all().await.map_err(io(&path))?;
    Ok(PromptPart::Saved {
        url: format!("local://{final_name}"),
        image: detect_image_mime(&bytes[..bytes.len().min(IMAGE_SNIFF_BYTES)]).is_some(),
    })
}

/// `name` for the first copy, else `stem-{copy}.ext` (split on the last `.`; dotfiles have no extension).
fn numbered(name: &str, copy: u32) -> String {
    if copy == 1 {
        return name.to_owned();
    }
    match name.rfind('.') {
        Some(dot) if dot > 0 => format!("{}-{copy}{}", &name[..dot], &name[dot..]),
        _ => format!("{name}-{copy}"),
    }
}

/// The model blocks for prepared parts: one text (a skill command expanded when the message
/// starts with typed `/` text), then each saved image as its URL plus pixels. Image failures
/// become text so the message still sends.
pub(crate) async fn render(
    parts: &[PromptPart],
    skills: &[Skill],
    tools: &SessionTools,
) -> Vec<UserBlock> {
    let text = match command_text(parts) {
        Some(text) => expand_skill_command(&text, skills),
        None => flatten(parts),
    };
    let mut blocks = vec![UserBlock::Text { text }];
    for part in parts {
        let PromptPart::Saved { url, image: true } = part else {
            continue;
        };
        match image_blocks(url, tools).await {
            Ok(image) => blocks.extend(image),
            Err(message) => blocks.push(UserBlock::Text {
                text: format!("{url}\n{message}"),
            }),
        }
    }
    blocks
}

/// `[Text(url + hints), Image]`, or the model-facing message when the image cannot be inlined.
async fn image_blocks(url: &str, tools: &SessionTools) -> Result<[UserBlock; 2], String> {
    let unavailable = |error: &dyn std::fmt::Display| format!("[image unavailable: {error}]");
    let path = match tools.locate(url).await {
        Ok(Located::File { path, .. }) => path,
        Ok(_) => return Err(unavailable(&"not a file")),
        Err(error) => return Err(unavailable(&error)),
    };
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|error| unavailable(&error))?;
    let mime = detect_image_mime(&bytes[..bytes.len().min(IMAGE_SNIFF_BYTES)])
        .ok_or_else(|| unavailable(&"not a supported image"))?;
    let image = tokio::task::spawn_blocking(move || process_image(&bytes, mime))
        .await
        .map_err(|error| unavailable(&error))??;
    let mut text = url.to_owned();
    for hint in &image.hints {
        text.push('\n');
        text.push_str(hint);
    }
    Ok([
        UserBlock::Text { text },
        UserBlock::Image {
            data: image.data.into(),
            mime_type: image.mime_type.to_string(),
        },
    ])
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parts_serialize_camel_case_with_base64_content() {
        let parts = vec![
            PromptPart::Paste { text: "p".into() },
            PromptPart::Content {
                name: "a.txt".into(),
                bytes: b"hi".to_vec(),
            },
            PromptPart::Saved {
                url: "local://a.txt".into(),
                image: false,
            },
        ];
        let value = serde_json::to_value(&parts).unwrap();
        assert_eq!(
            value,
            json!([
                {"type": "paste", "text": "p"},
                {"type": "content", "name": "a.txt", "bytes": "aGk="},
                {"type": "saved", "url": "local://a.txt", "image": false},
            ])
        );
        assert_eq!(
            serde_json::from_value::<Vec<PromptPart>>(value).unwrap(),
            parts
        );
    }

    #[test]
    fn command_text_flattens_references() {
        let parts = vec![
            PromptPart::Text {
                text: "/review ".into(),
            },
            PromptPart::File {
                path: "my notes.txt".into(),
            },
        ];
        assert_eq!(
            command_text(&parts).as_deref(),
            Some("/review @\"my notes.txt\"")
        );
        assert_eq!(command_text(&parts[1..]), None);
    }

    #[test]
    fn numbered_names_keep_extension() {
        assert_eq!(numbered("shot.png", 1), "shot.png");
        assert_eq!(numbered("shot.png", 2), "shot-2.png");
        assert_eq!(numbered("a.tar.gz", 3), "a.tar-3.gz");
        assert_eq!(numbered(".env", 2), ".env-2");
        assert_eq!(numbered("README", 2), "README-2");
    }
}
