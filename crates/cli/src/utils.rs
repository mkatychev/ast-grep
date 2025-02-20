use crate::error::ErrorContext as EC;
use crate::lang::SgLang;
use anyhow::{anyhow, Context, Result};
use crossterm::{
  event::{self, Event, KeyCode},
  execute,
  terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ignore::{DirEntry, WalkParallel, WalkState};

use ast_grep_core::{AstGrep, Matcher, StrDoc};
use ast_grep_language::Language;

use std::fs::read_to_string;
use std::io::stdout;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

fn read_char() -> Result<char> {
  loop {
    if let Event::Key(evt) = event::read()? {
      match evt.code {
        KeyCode::Enter => break Ok('\n'),
        KeyCode::Char(c) => break Ok(c),
        _ => (),
      }
    }
  }
}

/// Prompts for user input on STDOUT
fn prompt_reply_stdout(prompt: &str) -> Result<char> {
  let mut stdout = std::io::stdout();
  write!(stdout, "{}", prompt)?;
  stdout.flush()?;
  terminal::enable_raw_mode()?;
  let ret = read_char();
  terminal::disable_raw_mode()?;
  ret
}

// https://github.com/console-rs/console/blob/be1c2879536c90ffc2b54938b5964084f5fef67d/src/common_term.rs#L56
// clear screen
fn clear() {
  print!("\r\x1b[2J\r\x1b[H");
}

pub fn run_in_alternate_screen<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
  execute!(stdout(), EnterAlternateScreen)?;
  clear();
  let ret = f();
  execute!(stdout(), LeaveAlternateScreen)?;
  ret
}

pub fn prompt(prompt_text: &str, letters: &str, default: Option<char>) -> Result<char> {
  loop {
    let input = prompt_reply_stdout(prompt_text)?;
    if let Some(default) = default {
      if input == '\n' {
        return Ok(default);
      }
    }
    if letters.contains(input) {
      return Ok(input);
    }
    println!("Unrecognized command, try again?")
  }
}

// TODO: add comment
pub trait Worker: Sync {
  type Item: Send;
  fn build_walk(&self) -> WalkParallel;
  fn produce_item(&self, path: &Path) -> Option<Self::Item>;
  fn consume_items(&self, items: Items<Self::Item>) -> Result<()>;
}

pub struct Items<T>(mpsc::Receiver<T>);
impl<T> Iterator for Items<T> {
  type Item = T;
  fn next(&mut self) -> Option<Self::Item> {
    if let Ok(match_result) = self.0.recv() {
      Some(match_result)
    } else {
      None
    }
  }
}

fn filter_result(result: Result<DirEntry, ignore::Error>) -> Option<PathBuf> {
  let entry = match result {
    Ok(entry) => entry,
    Err(err) => {
      eprintln!("ERROR: {}", err);
      return None;
    }
  };
  entry.file_type()?.is_file().then(|| entry.into_path())
}

pub fn run_worker<MW: Worker>(worker: MW) -> Result<()> {
  let producer = |path: PathBuf| worker.produce_item(&path);
  let (tx, rx) = mpsc::channel();
  let walker = worker.build_walk();
  walker.run(|| {
    let tx = tx.clone();
    Box::new(move |result| {
      let maybe_result = filter_result(result).and_then(producer);
      let result = match maybe_result {
        Some(ret) => ret,
        None => return WalkState::Continue,
      };
      match tx.send(result) {
        Ok(_) => WalkState::Continue,
        Err(_) => WalkState::Quit,
      }
    })
  });
  // drop the last sender to stop rx awaiting message
  drop(tx);
  worker.consume_items(Items(rx))
}

pub fn open_in_editor(path: &PathBuf, start_line: usize) -> Result<()> {
  let editor = std::env::var("EDITOR").unwrap_or_else(|_| String::from("vim"));
  let exit = std::process::Command::new(editor)
    .arg(path)
    .arg(format!("+{}", start_line))
    .spawn()
    .context(EC::OpenEditor)?
    .wait()
    .context(EC::OpenEditor)?;
  if exit.success() {
    Ok(())
  } else {
    Err(anyhow!(EC::OpenEditor))
  }
}

pub fn filter_file_interactive<M: Matcher<SgLang>>(
  path: &Path,
  lang: SgLang,
  matcher: M,
) -> Option<MatchUnit<M>> {
  let file_content = read_to_string(path)
    .with_context(|| format!("Cannot read file {}", path.to_string_lossy()))
    .map_err(|err| eprintln!("{err}"))
    .ok()?;
  // skip large files or empty file
  if file_too_large(&file_content) || file_content.is_empty() {
    // TODO add output
    return None;
  }
  let grep = lang.ast_grep(file_content);
  let has_match = grep.root().find(&matcher).is_some();
  has_match.then(|| MatchUnit {
    grep,
    path: path.to_path_buf(),
    matcher,
  })
}

const MAX_FILE_SIZE: usize = 3_000_000;
const MAX_LINE_COUNT: usize = 200_000;

fn file_too_large(file_content: &String) -> bool {
  file_content.len() > MAX_FILE_SIZE && file_content.lines().count() > MAX_LINE_COUNT
}

/// A single atomic unit where matches happen.
/// It contains the file path, sg instance and matcher.
/// An analogy to compilation unit in C programming language.
pub struct MatchUnit<M: Matcher<SgLang>> {
  pub path: PathBuf,
  pub grep: AstGrep<StrDoc<SgLang>>,
  pub matcher: M,
}

#[cfg(test)]
mod test {
  use super::*;

  #[test]
  fn test_open_editor() {
    // these two tests must run in sequence
    // since setting env will cause racing condition
    test_open_editor_respect_editor_env();
    test_open_editor_error_handling();
  }

  fn test_open_editor_respect_editor_env() {
    std::env::set_var("EDITOR", "echo");
    let exit = open_in_editor(&PathBuf::from("Cargo.toml"), 1);
    assert!(exit.is_ok());
  }

  fn test_open_editor_error_handling() {
    std::env::set_var("EDITOR", "NOT_EXIST_XXXXX");
    let exit = open_in_editor(&PathBuf::from("Cargo.toml"), 1);
    let error = exit.expect_err("should be error");
    let error = error.downcast_ref::<EC>().expect("should be error context");
    assert!(matches!(error, EC::OpenEditor));
  }
}
