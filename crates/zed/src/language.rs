use anyhow::{anyhow, Context, Result};
use async_compression::futures::bufread::GzipDecoder;
use client::http::{self, HttpClient, Method};
use futures::{future::BoxFuture, FutureExt, StreamExt};
pub use language::*;
use lazy_static::lazy_static;
use regex::Regex;
use rust_embed::RustEmbed;
use serde::Deserialize;
use smol::fs::{self, File};
use std::{
    borrow::Cow,
    env::consts,
    path::{Path, PathBuf},
    str,
    sync::Arc,
};
use util::{ResultExt, TryFutureExt};

#[derive(RustEmbed)]
#[folder = "languages"]
struct LanguageDir;

struct RustLsp;
struct CLsp;

#[derive(Deserialize)]
struct GithubRelease {
    name: String,
    assets: Vec<GithubReleaseAsset>,
}

#[derive(Deserialize)]
struct GithubReleaseAsset {
    name: String,
    browser_download_url: http::Url,
}

impl LspExt for RustLsp {
    fn fetch_latest_server_version(
        &self,
        http: Arc<dyn HttpClient>,
    ) -> BoxFuture<'static, Result<LspBinaryVersion>> {
        async move {
            let release = http
            .send(
                surf::RequestBuilder::new(
                    Method::Get,
                    http::Url::parse(
                        "https://api.github.com/repos/rust-analyzer/rust-analyzer/releases/latest",
                    )
                    .unwrap(),
                )
                .middleware(surf::middleware::Redirect::default())
                .build(),
            )
            .await
            .map_err(|err| anyhow!("error fetching latest release: {}", err))?
            .body_json::<GithubRelease>()
            .await
            .map_err(|err| anyhow!("error parsing latest release: {}", err))?;
            let asset_name = format!("rust-analyzer-{}-apple-darwin.gz", consts::ARCH);
            let asset = release
                .assets
                .iter()
                .find(|asset| asset.name == asset_name)
                .ok_or_else(|| anyhow!("no release found matching {:?}", asset_name))?;
            Ok(LspBinaryVersion {
                name: release.name,
                url: asset.browser_download_url.clone(),
            })
        }
        .boxed()
    }

    fn fetch_server_binary(
        &self,
        version: LspBinaryVersion,
        http: Arc<dyn HttpClient>,
        download_dir: Arc<Path>,
    ) -> BoxFuture<'static, Result<PathBuf>> {
        async move {
            let destination_dir_path = download_dir.join("rust-analyzer");
            fs::create_dir_all(&destination_dir_path).await?;
            let destination_path =
                destination_dir_path.join(format!("rust-analyzer-{}", version.name));

            if fs::metadata(&destination_path).await.is_err() {
                let response = http
                    .send(
                        surf::RequestBuilder::new(Method::Get, version.url)
                            .middleware(surf::middleware::Redirect::default())
                            .build(),
                    )
                    .await
                    .map_err(|err| anyhow!("error downloading release: {}", err))?;
                let decompressed_bytes = GzipDecoder::new(response);
                let mut file = File::create(&destination_path).await?;
                futures::io::copy(decompressed_bytes, &mut file).await?;
                fs::set_permissions(
                    &destination_path,
                    <fs::Permissions as fs::unix::PermissionsExt>::from_mode(0o755),
                )
                .await?;

                if let Some(mut entries) = fs::read_dir(&destination_dir_path).await.log_err() {
                    while let Some(entry) = entries.next().await {
                        if let Some(entry) = entry.log_err() {
                            let entry_path = entry.path();
                            if entry_path.as_path() != destination_path {
                                fs::remove_file(&entry_path).await.log_err();
                            }
                        }
                    }
                }
            }

            Ok(destination_path)
        }
        .boxed()
    }

    fn cached_server_binary(&self, download_dir: Arc<Path>) -> BoxFuture<'static, Option<PathBuf>> {
        async move {
            let destination_dir_path = download_dir.join("rust-analyzer");
            fs::create_dir_all(&destination_dir_path).await?;

            let mut last = None;
            let mut entries = fs::read_dir(&destination_dir_path).await?;
            while let Some(entry) = entries.next().await {
                last = Some(entry?.path());
            }
            last.ok_or_else(|| anyhow!("no cached binary"))
        }
        .log_err()
        .boxed()
    }

    fn process_diagnostics(&self, params: &mut lsp::PublishDiagnosticsParams) {
        lazy_static! {
            static ref REGEX: Regex = Regex::new("(?m)`([^`]+)\n`$").unwrap();
        }

        for diagnostic in &mut params.diagnostics {
            for message in diagnostic
                .related_information
                .iter_mut()
                .flatten()
                .map(|info| &mut info.message)
                .chain([&mut diagnostic.message])
            {
                if let Cow::Owned(sanitized) = REGEX.replace_all(message, "`$1`") {
                    *message = sanitized;
                }
            }
        }
    }

    fn label_for_completion(
        &self,
        completion: &lsp::CompletionItem,
        language: &Language,
    ) -> Option<CompletionLabel> {
        match completion.kind {
            Some(lsp::CompletionItemKind::FIELD) if completion.detail.is_some() => {
                let detail = completion.detail.as_ref().unwrap();
                let name = &completion.label;
                let text = format!("{}: {}", name, detail);
                let source = Rope::from(format!("struct S {{ {} }}", text).as_str());
                let runs = language.highlight_text(&source, 11..11 + text.len());
                return Some(CompletionLabel {
                    text,
                    runs,
                    filter_range: 0..name.len(),
                    left_aligned_len: name.len(),
                });
            }
            Some(lsp::CompletionItemKind::CONSTANT | lsp::CompletionItemKind::VARIABLE)
                if completion.detail.is_some() =>
            {
                let detail = completion.detail.as_ref().unwrap();
                let name = &completion.label;
                let text = format!("{}: {}", name, detail);
                let source = Rope::from(format!("let {} = ();", text).as_str());
                let runs = language.highlight_text(&source, 4..4 + text.len());
                return Some(CompletionLabel {
                    text,
                    runs,
                    filter_range: 0..name.len(),
                    left_aligned_len: name.len(),
                });
            }
            Some(lsp::CompletionItemKind::FUNCTION | lsp::CompletionItemKind::METHOD)
                if completion.detail.is_some() =>
            {
                lazy_static! {
                    static ref REGEX: Regex = Regex::new("\\(…?\\)").unwrap();
                }

                let detail = completion.detail.as_ref().unwrap();
                if detail.starts_with("fn(") {
                    let text = REGEX.replace(&completion.label, &detail[2..]).to_string();
                    let source = Rope::from(format!("fn {} {{}}", text).as_str());
                    let runs = language.highlight_text(&source, 3..3 + text.len());
                    return Some(CompletionLabel {
                        left_aligned_len: text.find("->").unwrap_or(text.len()),
                        filter_range: 0..completion.label.find('(').unwrap_or(text.len()),
                        text,
                        runs,
                    });
                }
            }
            Some(kind) => {
                let highlight_name = match kind {
                    lsp::CompletionItemKind::STRUCT
                    | lsp::CompletionItemKind::INTERFACE
                    | lsp::CompletionItemKind::ENUM => Some("type"),
                    lsp::CompletionItemKind::ENUM_MEMBER => Some("variant"),
                    lsp::CompletionItemKind::KEYWORD => Some("keyword"),
                    lsp::CompletionItemKind::VALUE | lsp::CompletionItemKind::CONSTANT => {
                        Some("constant")
                    }
                    _ => None,
                };
                let highlight_id = language.grammar()?.highlight_id_for_name(highlight_name?)?;
                let mut label = CompletionLabel::plain(&completion);
                label.runs.push((
                    0..label.text.rfind('(').unwrap_or(label.text.len()),
                    highlight_id,
                ));
                return Some(label);
            }
            _ => {}
        }
        None
    }
}

impl LspExt for CLsp {
    fn fetch_latest_server_version(
        &self,
        http: Arc<dyn HttpClient>,
    ) -> BoxFuture<'static, Result<LspBinaryVersion>> {
        async move {
            let release = http
                .send(
                    surf::RequestBuilder::new(
                        Method::Get,
                        http::Url::parse(
                            "https://api.github.com/repos/clangd/clangd/releases/latest",
                        )
                        .unwrap(),
                    )
                    .middleware(surf::middleware::Redirect::default())
                    .build(),
                )
                .await
                .map_err(|err| anyhow!("error fetching latest release: {}", err))?
                .body_json::<GithubRelease>()
                .await
                .map_err(|err| anyhow!("error parsing latest release: {}", err))?;
            let asset_name = format!("clangd-mac-{}.zip", release.name);
            let asset = release
                .assets
                .iter()
                .find(|asset| asset.name == asset_name)
                .ok_or_else(|| anyhow!("no release found matching {:?}", asset_name))?;
            Ok(LspBinaryVersion {
                name: release.name,
                url: asset.browser_download_url.clone(),
            })
        }
        .boxed()
    }

    fn fetch_server_binary(
        &self,
        version: LspBinaryVersion,
        http: Arc<dyn HttpClient>,
        download_dir: Arc<Path>,
    ) -> BoxFuture<'static, Result<PathBuf>> {
        async move {
            let container_dir = download_dir.join("clangd");
            fs::create_dir_all(&container_dir)
                .await
                .context("failed to create container directory")?;

            let zip_path = container_dir.join(format!("clangd_{}.zip", version.name));
            let version_dir = container_dir.join(format!("clangd_{}", version.name));
            let binary_path = version_dir.join("bin/clangd");

            if fs::metadata(&binary_path).await.is_err() {
                let response = http
                    .send(
                        surf::RequestBuilder::new(Method::Get, version.url)
                            .middleware(surf::middleware::Redirect::default())
                            .build(),
                    )
                    .await
                    .map_err(|err| anyhow!("error downloading release: {}", err))?;
                let mut file = File::create(&zip_path).await?;
                if !response.status().is_success() {
                    Err(anyhow!(
                        "download failed with status {}",
                        response.status().to_string()
                    ))?;
                }
                futures::io::copy(response, &mut file).await?;

                let unzip_status = smol::process::Command::new("unzip")
                    .current_dir(&container_dir)
                    .arg(&zip_path)
                    .output()
                    .await?
                    .status;
                if !unzip_status.success() {
                    Err(anyhow!("failed to unzip clangd archive"))?;
                }

                if let Some(mut entries) = fs::read_dir(&container_dir).await.log_err() {
                    while let Some(entry) = entries.next().await {
                        if let Some(entry) = entry.log_err() {
                            let entry_path = entry.path();
                            if entry_path.as_path() != version_dir {
                                fs::remove_dir_all(&entry_path).await.log_err();
                            }
                        }
                    }
                }
            }

            Ok(binary_path)
        }
        .boxed()
    }

    fn cached_server_binary(&self, download_dir: Arc<Path>) -> BoxFuture<'static, Option<PathBuf>> {
        async move {
            let destination_dir_path = download_dir.join("clangd");
            fs::create_dir_all(&destination_dir_path).await?;

            let mut last_clangd_dir = None;
            let mut entries = fs::read_dir(&destination_dir_path).await?;
            while let Some(entry) = entries.next().await {
                let entry = entry?;
                if entry.file_type().await?.is_dir() {
                    last_clangd_dir = Some(entry.path());
                }
            }
            let clangd_dir = last_clangd_dir.ok_or_else(|| anyhow!("no cached binary"))?;
            let clangd_bin = clangd_dir.join("bin/clangd");
            if clangd_bin.exists() {
                Ok(clangd_bin)
            } else {
                Err(anyhow!(
                    "missing clangd binary in directory {:?}",
                    clangd_dir
                ))
            }
        }
        .log_err()
        .boxed()
    }

    fn process_diagnostics(&self, _: &mut lsp::PublishDiagnosticsParams) {}
}

pub fn build_language_registry() -> LanguageRegistry {
    let mut languages = LanguageRegistry::new();
    languages.set_language_server_download_dir(
        dirs::home_dir()
            .expect("failed to determine home directory")
            .join(".zed"),
    );
    languages.add(Arc::new(c()));
    languages.add(Arc::new(rust()));
    languages.add(Arc::new(markdown()));
    languages
}

fn rust() -> Language {
    let grammar = tree_sitter_rust::language();
    let config = toml::from_slice(&LanguageDir::get("rust/config.toml").unwrap().data).unwrap();
    Language::new(config, Some(grammar))
        .with_highlights_query(load_query("rust/highlights.scm").as_ref())
        .unwrap()
        .with_brackets_query(load_query("rust/brackets.scm").as_ref())
        .unwrap()
        .with_indents_query(load_query("rust/indents.scm").as_ref())
        .unwrap()
        .with_outline_query(load_query("rust/outline.scm").as_ref())
        .unwrap()
        .with_lsp_ext(RustLsp)
}

fn c() -> Language {
    let grammar = tree_sitter_c::language();
    let config = toml::from_slice(&LanguageDir::get("c/config.toml").unwrap().data).unwrap();
    Language::new(config, Some(grammar))
        .with_highlights_query(load_query("c/highlights.scm").as_ref())
        .unwrap()
        .with_indents_query(load_query("c/indents.scm").as_ref())
        .unwrap()
        .with_outline_query(load_query("c/outline.scm").as_ref())
        .unwrap()
        .with_lsp_ext(CLsp)
}

fn markdown() -> Language {
    let grammar = tree_sitter_markdown::language();
    let config = toml::from_slice(&LanguageDir::get("markdown/config.toml").unwrap().data).unwrap();
    Language::new(config, Some(grammar))
        .with_highlights_query(load_query("markdown/highlights.scm").as_ref())
        .unwrap()
}

fn load_query(path: &str) -> Cow<'static, str> {
    match LanguageDir::get(path).unwrap().data {
        Cow::Borrowed(s) => Cow::Borrowed(str::from_utf8(s).unwrap()),
        Cow::Owned(s) => Cow::Owned(String::from_utf8(s).unwrap()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::color::Color;
    use language::LspExt;
    use theme::SyntaxTheme;

    #[test]
    fn test_process_rust_diagnostics() {
        let mut params = lsp::PublishDiagnosticsParams {
            uri: lsp::Url::from_file_path("/a").unwrap(),
            version: None,
            diagnostics: vec![
                // no newlines
                lsp::Diagnostic {
                    message: "use of moved value `a`".to_string(),
                    ..Default::default()
                },
                // newline at the end of a code span
                lsp::Diagnostic {
                    message: "consider importing this struct: `use b::c;\n`".to_string(),
                    ..Default::default()
                },
                // code span starting right after a newline
                lsp::Diagnostic {
                    message: "cannot borrow `self.d` as mutable\n`self` is a `&` reference"
                        .to_string(),
                    ..Default::default()
                },
            ],
        };
        RustLsp.process_diagnostics(&mut params);

        assert_eq!(params.diagnostics[0].message, "use of moved value `a`");

        // remove trailing newline from code span
        assert_eq!(
            params.diagnostics[1].message,
            "consider importing this struct: `use b::c;`"
        );

        // do not remove newline before the start of code span
        assert_eq!(
            params.diagnostics[2].message,
            "cannot borrow `self.d` as mutable\n`self` is a `&` reference"
        );
    }

    #[test]
    fn test_process_rust_completions() {
        let language = rust();
        let grammar = language.grammar().unwrap();
        let theme = SyntaxTheme::new(vec![
            ("type".into(), Color::green().into()),
            ("keyword".into(), Color::blue().into()),
            ("function".into(), Color::red().into()),
            ("property".into(), Color::white().into()),
        ]);

        language.set_theme(&theme);

        let highlight_function = grammar.highlight_id_for_name("function").unwrap();
        let highlight_type = grammar.highlight_id_for_name("type").unwrap();
        let highlight_keyword = grammar.highlight_id_for_name("keyword").unwrap();
        let highlight_field = grammar.highlight_id_for_name("property").unwrap();

        assert_eq!(
            language.label_for_completion(&lsp::CompletionItem {
                kind: Some(lsp::CompletionItemKind::FUNCTION),
                label: "hello(…)".to_string(),
                detail: Some("fn(&mut Option<T>) -> Vec<T>".to_string()),
                ..Default::default()
            }),
            Some(CompletionLabel {
                text: "hello(&mut Option<T>) -> Vec<T>".to_string(),
                filter_range: 0..5,
                runs: vec![
                    (0..5, highlight_function),
                    (7..10, highlight_keyword),
                    (11..17, highlight_type),
                    (18..19, highlight_type),
                    (25..28, highlight_type),
                    (29..30, highlight_type),
                ],
                left_aligned_len: 22,
            })
        );

        assert_eq!(
            language.label_for_completion(&lsp::CompletionItem {
                kind: Some(lsp::CompletionItemKind::FIELD),
                label: "len".to_string(),
                detail: Some("usize".to_string()),
                ..Default::default()
            }),
            Some(CompletionLabel {
                text: "len: usize".to_string(),
                filter_range: 0..3,
                runs: vec![(0..3, highlight_field), (5..10, highlight_type),],
                left_aligned_len: 3,
            })
        );

        assert_eq!(
            language.label_for_completion(&lsp::CompletionItem {
                kind: Some(lsp::CompletionItemKind::FUNCTION),
                label: "hello(…)".to_string(),
                detail: Some("fn(&mut Option<T>) -> Vec<T>".to_string()),
                ..Default::default()
            }),
            Some(CompletionLabel {
                text: "hello(&mut Option<T>) -> Vec<T>".to_string(),
                filter_range: 0..5,
                runs: vec![
                    (0..5, highlight_function),
                    (7..10, highlight_keyword),
                    (11..17, highlight_type),
                    (18..19, highlight_type),
                    (25..28, highlight_type),
                    (29..30, highlight_type),
                ],
                left_aligned_len: 22,
            })
        );
    }
}
