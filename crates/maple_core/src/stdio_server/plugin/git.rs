use crate::stdio_server::input::AutocmdEventType;
use crate::stdio_server::plugin::{
    Action, ActionType, ClapAction, ClapPlugin, PluginAction, PluginEvent, PluginId,
};
use crate::stdio_server::vim::Vim;
use anyhow::{anyhow, Result};
use chrono::{TimeZone, Utc};
use itertools::Itertools;
use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

#[derive(Debug, Clone)]
struct Git {
    repo: PathBuf,
    user_name: String,
}

impl Git {
    fn init(git_root: PathBuf) -> Result<Self> {
        let output = std::process::Command::new("git")
            .current_dir(&git_root)
            .arg("config")
            .arg("user.name")
            .stderr(Stdio::null())
            .output()?;

        let user_name = String::from_utf8(output.stdout)?.trim().to_string();

        Ok(Self {
            repo: git_root,
            user_name,
        })
    }

    fn fetch_rev_parse(&self, arg: &str) -> Result<String> {
        let output = std::process::Command::new("git")
            .current_dir(&self.repo)
            .arg("rev-parse")
            .arg(arg)
            .stderr(Stdio::null())
            .output()?;

        Ok(String::from_utf8(output.stdout)?)
    }

    #[allow(unused)]
    fn fetch_user_name(&self) -> Result<String> {
        let output = std::process::Command::new("git")
            .current_dir(&self.repo)
            .arg("config")
            .arg("user.name")
            .stderr(Stdio::null())
            .output()?;

        Ok(String::from_utf8(output.stdout)?)
    }

    fn fetch_origin_url(&self) -> Result<String> {
        let output = std::process::Command::new("git")
            .current_dir(&self.repo)
            .arg("config")
            .arg("--get")
            .arg("remote.origin.url")
            .stderr(Stdio::null())
            .output()?;

        Ok(String::from_utf8(output.stdout)?)
    }

    fn fetch_blame_output(&self, relative_path: &Path, lnum: usize) -> Result<Vec<u8>> {
        let output = std::process::Command::new("git")
            .current_dir(&self.repo)
            .arg("blame")
            .arg("--porcelain")
            .arg("--incremental")
            .arg(format!("-L{lnum},{lnum}"))
            .arg("--")
            .arg(relative_path)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()?;

        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(anyhow!(
                "Child process errors out: {}",
                String::from_utf8_lossy(&output.stderr)
            ))
        }
    }

    // git blame --contents - -L 100,+1 --line-porcelain crates/maple_core/src/stdio_server/plugin/git.rs
    fn fetch_blame_output_with_lines(
        &self,
        relative_path: &Path,
        lnum: usize,
        lines: Vec<String>,
    ) -> Result<Vec<u8>> {
        let mut p = std::process::Command::new("git")
            .current_dir(&self.repo)
            .arg("blame")
            .arg("--contents")
            .arg("-")
            .arg("-L")
            .arg(format!("{lnum},+1"))
            .arg("--line-porcelain")
            .arg(relative_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        let lines = lines.into_iter().join("\n");
        let stdin = p
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("stdin unavailable"))?;
        stdin.write_all(lines.as_bytes())?;

        let output = p.wait_with_output()?;

        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(anyhow!(
                "Child process errors out: {}",
                String::from_utf8_lossy(&output.stderr)
            ))
        }
    }
}

struct BlameInfo {
    author: String,
    author_time: Option<i64>,
    summary: Option<String>,
}

impl BlameInfo {
    fn display(&self, user_name: &str) -> Result<Cow<'_, str>> {
        let author = &self.author;

        if author == "Not Committed Yet" {
            return Ok(author.into());
        }

        match (&self.author_time, &self.summary) {
            (Some(author_time), Some(summary)) => {
                let time = Utc
                    .timestamp_opt(*author_time, 0)
                    .single()
                    .ok_or_else(|| anyhow!("Failed to parse timestamp {author_time}"))?;
                let time = chrono_humanize::HumanTime::from(time);
                let author = if user_name.eq(author) { "You" } else { author };

                if let Some(fmt) = &crate::config::config().plugin.git.blame_format_string {
                    let mut display_string = fmt.to_string();
                    let mut replace_template_string = |to_replace: &str, replace_with: &str| {
                        if let Some(idx) = display_string.find(to_replace) {
                            display_string.replace_range(idx..idx + to_replace.len(), replace_with);
                        }
                    };

                    replace_template_string("author", author);
                    replace_template_string("time", time.to_string().as_str());
                    replace_template_string("summary", summary);

                    Ok(display_string.into())
                } else {
                    Ok(format!("({author} {time}) {summary}").into())
                }
            }
            _ => Ok(format!("({author})").into()),
        }
    }
}

fn parse_blame_info(stdout: Vec<u8>) -> Result<Option<BlameInfo>> {
    let stdout = String::from_utf8_lossy(&stdout);

    let mut author = None;
    let mut author_time = None;
    let mut summary = None;

    for line in stdout.split('\n') {
        if let Some((k, v)) = line.split_once(' ') {
            match k {
                "author" => {
                    author.replace(v);
                }
                "author-time" => {
                    author_time.replace(v);
                }
                "summary" => {
                    summary.replace(v);
                }
                _ => {}
            }
        }

        if let (Some(author), Some(author_time), Some(summary)) = (author, author_time, summary) {
            return Ok(Some(BlameInfo {
                author: author.to_owned(),
                author_time: Some(author_time.parse::<i64>()?),
                summary: Some(summary.to_owned()),
            }));
        }
    }

    Ok(None)
}

fn in_git_repo(filepath: &Path) -> Option<&Path> {
    filepath
        .exists()
        .then(|| paths::find_git_root(filepath))
        .flatten()
}

#[derive(Debug, Clone)]
enum Toggle {
    On,
    Off,
}

impl Toggle {
    fn switch(&mut self) {
        match self {
            Self::On => {
                *self = Self::Off;
            }
            Self::Off => {
                *self = Self::On;
            }
        }
    }

    fn is_off(&self) -> bool {
        matches!(self, Self::Off)
    }
}

#[derive(Debug, Clone)]
pub struct GitPlugin {
    vim: Vim,
    bufs: HashMap<usize, (PathBuf, Git)>,
    toggle: Toggle,
}

impl GitPlugin {
    pub const ID: PluginId = PluginId::Git;

    const BLAME: &'static str = "git/blame";
    const BLAME_ACTION: Action = Action::callable(Self::BLAME);

    const OPEN_CURRENT_LINE_IN_BROWSER: &'static str = "git/open-current-line-in-browser";
    const OPEN_CURRENT_LINE_IN_BROWSER_ACTION: Action =
        Action::callable(Self::OPEN_CURRENT_LINE_IN_BROWSER);

    const TOGGLE: &'static str = "git/toggle";
    const TOGGLE_ACTION: Action = Action::callable(Self::TOGGLE);

    const ACTIONS: &[Action] = &[
        Self::BLAME_ACTION,
        Self::OPEN_CURRENT_LINE_IN_BROWSER_ACTION,
        Self::TOGGLE_ACTION,
    ];

    pub fn new(vim: Vim) -> Self {
        Self {
            vim,
            bufs: HashMap::new(),
            toggle: Toggle::On,
        }
    }

    async fn try_track_buffer(&mut self, bufnr: usize) -> Result<()> {
        if self.bufs.contains_key(&bufnr) {
            return Ok(());
        }

        let buf_path = self.vim.current_buffer_path().await?;

        let filepath = PathBuf::from(buf_path);

        if let Some(git_root) = in_git_repo(&filepath) {
            let git_root = git_root.to_path_buf();
            self.bufs.insert(bufnr, (filepath, Git::init(git_root)?));
            return Ok(());
        }

        Ok(())
    }

    async fn on_cursor_moved(&self, bufnr: usize) -> Result<()> {
        if let Some((filepath, git)) = self.bufs.get(&bufnr) {
            let maybe_blame_info = self.cursor_line_blame_info(git, filepath).await?;
            if let Some(blame_info) = maybe_blame_info {
                self.vim.exec(
                    "clap#plugin#git#show_cursor_blame_info",
                    (bufnr, blame_info),
                )?;
            }
        }
        Ok(())
    }

    async fn cursor_line_blame_info(&self, git: &Git, filepath: &Path) -> Result<Option<String>> {
        let relative_path = filepath.strip_prefix(&git.repo)?;

        let lnum = self.vim.line(".").await?;

        let stdout = if self.vim.bufmodified("").await? {
            let lines = self.vim.getbufline("", 1, "$").await?;
            git.fetch_blame_output_with_lines(relative_path, lnum, lines)?
        } else {
            git.fetch_blame_output(relative_path, lnum)?
        };

        if let Ok(Some(blame_info)) = parse_blame_info(stdout) {
            return Ok(Some(blame_info.display(&git.user_name)?.to_string()));
        }

        Ok(None)
    }

    async fn show_blame_info(&self) -> Result<()> {
        let buf_path = self.vim.current_buffer_path().await?;
        let filepath = PathBuf::from(buf_path);

        let Some(git_root) = in_git_repo(&filepath) else {
            return Ok(());
        };

        if let Ok(Some(blame_info)) = self
            .cursor_line_blame_info(&Git::init(git_root.to_path_buf())?, &filepath)
            .await
        {
            self.vim.echo_info(blame_info)?;
        }

        Ok(())
    }
}

impl ClapAction for GitPlugin {
    fn actions(&self, _action_type: ActionType) -> &[Action] {
        Self::ACTIONS
    }
}

#[async_trait::async_trait]
impl ClapPlugin for GitPlugin {
    fn id(&self) -> PluginId {
        Self::ID
    }

    async fn on_plugin_event(&mut self, plugin_event: PluginEvent) -> Result<()> {
        match plugin_event {
            PluginEvent::Autocmd((autocmd_event_type, params)) => {
                use AutocmdEventType::{BufDelete, BufEnter, CursorMoved, InsertEnter};

                if self.toggle.is_off() {
                    return Ok(());
                }

                let bufnr = params.parse_bufnr()?;

                match autocmd_event_type {
                    BufEnter => {
                        self.try_track_buffer(bufnr).await?;
                        self.on_cursor_moved(bufnr).await?;
                    }
                    BufDelete => {
                        self.bufs.remove(&bufnr);
                    }
                    InsertEnter => {
                        self.vim.exec("clap#plugin#git#clear_blame_info", [bufnr])?;
                    }
                    CursorMoved => self.on_cursor_moved(bufnr).await?,
                    _ => {}
                }

                Ok(())
            }
            PluginEvent::Action(plugin_action) => {
                let PluginAction { method, params: _ } = plugin_action;
                match method.as_str() {
                    Self::TOGGLE => {
                        match self.toggle {
                            Toggle::On => {
                                for bufnr in self.bufs.keys() {
                                    self.vim.exec("clap#plugin#git#clear_blame_info", [bufnr])?;
                                }
                            }
                            Toggle::Off => {
                                let bufnr = self.vim.bufnr("").await?;

                                self.on_cursor_moved(bufnr).await?;
                            }
                        }
                        self.toggle.switch();
                    }
                    Self::OPEN_CURRENT_LINE_IN_BROWSER => {
                        let buf_path = self.vim.current_buffer_path().await?;
                        let filepath = PathBuf::from(buf_path);

                        let Some(git_root) = in_git_repo(&filepath) else {
                            return Ok(());
                        };

                        let git = Git::init(git_root.to_path_buf())?;

                        let relative_path = filepath.strip_prefix(&git.repo)?;

                        let stdout = git.fetch_origin_url()?;
                        let remote_url = stdout.trim();

                        // https://github.com/liuchengxu/vim-clap{.git}
                        let remote_url = remote_url.strip_suffix(".git").unwrap_or(remote_url);

                        let Ok(stdout) = git.fetch_rev_parse("HEAD") else {
                            return Ok(());
                        };

                        let Some(rev) = stdout.split('\n').next() else {
                            return Ok(());
                        };

                        let lnum = self.vim.line(".").await?;
                        let commit_url = format!(
                            "{remote_url}/blob/{rev}/{}#L{lnum}",
                            relative_path.display()
                        );

                        if let Err(e) = webbrowser::open(&commit_url) {
                            self.vim
                                .echo_warn(format!("Failed to open {commit_url}: {e:?}"))?;
                        }
                    }
                    Self::BLAME => self.show_blame_info().await?,
                    unknown_action => return Err(anyhow!("Unknown action: {unknown_action:?}")),
                }

                Ok(())
            }
        }
    }
}
