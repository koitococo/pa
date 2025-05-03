use std::{collections::HashMap, fs::File, io::BufReader, process};

use anyhow::Result;
use clap::Parser;
use colored::Colorize as _;
use serde::Deserialize;
use time::{OffsetDateTime, format_description::FormatItem};
use tokio::{
  io::{AsyncBufReadExt, BufReader as AsyncBufReader},
  process::{ChildStderr, ChildStdout, Command},
  select,
  task::JoinSet,
};
use tokio_util::sync::CancellationToken;

const TIMESTAMP_FORMAT_OFFSET: &[FormatItem] =
  time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3][offset_hour sign:mandatory]:[offset_minute]");

fn colored_name(name: &str, index: usize) -> String {
  let name = name.to_string();
  let colored_str = match index % 6 {
    0 => name.bright_purple(),
    1 => name.bright_green(),
    2 => name.bright_yellow(),
    3 => name.bright_blue(),
    4 => name.bright_magenta(),
    5 => name.bright_cyan(),
    _ => unreachable!(),
  };
  colored_str.to_string()
}

#[derive(Parser, Debug)]
struct Args {
  #[clap(short, long, env, default_value = "pa.json")]
  config_file: String,
}

#[derive(Debug, Deserialize, Clone)]
struct Job {
  name: String,
  command: String,
  args: Option<Vec<String>>,
  env: Option<HashMap<String, String>>,
  inherit_env: Option<bool>,
}

#[derive(Debug, Deserialize, Clone)]
struct Config {
  jobs: Vec<Job>,
}

impl Config {
  fn from_file(file: &str) -> Result<Self> {
    let file = File::open(file)?;
    let reader = BufReader::new(file);
    let config: Config = serde_json::from_reader(reader)?;
    Ok(config)
  }
}

impl Into<Command> for Job {
  fn into(self) -> Command {
    let mut command = Command::new(self.command);
    if let Some(args) = self.args {
      command.args(args);
    }
    if let Some(false) = self.inherit_env {
      command.env_clear();
    }
    if let Some(envs) = self.env {
      command.envs(envs);
    }
    command.stdin(process::Stdio::null());
    command.stdout(process::Stdio::piped());
    command.stderr(process::Stdio::piped());
    command
  }
}

trait OptionExt<T> {
  fn ok(self) -> Result<T>;
}

impl<T> OptionExt<T> for Option<T> {
  fn ok(self) -> Result<T> {
    match self {
      Some(value) => Ok(value),
      None => Err(anyhow::anyhow!("Option was None")),
    }
  }
}

fn print(name: &str, line: String, is_err: bool) {
  let Ok(Ok(timestamp)) = OffsetDateTime::now_local().map(|t| t.format(TIMESTAMP_FORMAT_OFFSET)) else {
    eprintln!("Failed to get local time");
    return;
  };

  let timestamp = format!("{timestamp:<29}").bright_black().to_string();
  let line = line.trim();
  let msg = format!("[{timestamp}] {name}:: {line}");

  if is_err {
    eprintln!("{}", msg);
  } else {
    println!("{}", msg);
  }
}

trait ChildStdoutExt {
  async fn reprint(&mut self, name: &str) -> Result<()>;
}

impl ChildStdoutExt for AsyncBufReader<ChildStdout> {
  async fn reprint(&mut self, name: &str) -> Result<()> {
    let mut buf = String::new();
    self.read_line(&mut buf).await?;
    if buf.is_empty() {
      return Ok(());
    }
    print(&name, buf, false);
    Ok(())
  }
}

impl ChildStdoutExt for AsyncBufReader<ChildStderr> {
  async fn reprint(&mut self, name: &str) -> Result<()> {
    let mut buf = String::new();
    self.read_line(&mut buf).await?;
    if buf.is_empty() {
      return Ok(());
    }
    print(name, buf, true);
    Ok(())
  }
}

trait CommandExt: Sized {
  async fn run(&mut self, ct: CancellationToken, name: String) -> Result<i32>;
  async fn take_run(mut self, ct: CancellationToken, name: String) -> Result<i32> { self.run(ct, name).await }
}

impl CommandExt for Command {
  async fn run(&mut self, ct: CancellationToken, name: String) -> Result<i32> {
    let mut child = self.spawn()?;
    let stdout = child.stdout.take().ok()?;
    let stderr = child.stderr.take().ok()?;

    let mut stdout_bufreader = AsyncBufReader::new(stdout);
    let mut stderr_bufreader = AsyncBufReader::new(stderr);

    loop {
      select! {
        _ = ct.cancelled() => {
          child.kill().await?;
          return Err(anyhow::anyhow!("Cancelled"));
        }
        _ = stdout_bufreader.reprint(&name) => (),
        _ = stderr_bufreader.reprint(&name) => (),
        status = child.wait() => {
          let code = status?.code().ok()?;
          print(name.as_str(), format!("Process exited with code: {code}"), true);
          return Ok(code);
        }
      }
    }
  }
}

async fn run_jobs(jobs: Vec<Job>) -> Result<()> {
  let ct = CancellationToken::new();
  let futures = jobs.iter().enumerate().map(|(index, job)| {
    let ct = ct.clone();
    let cmd: Command = job.clone().into();
    let name = colored_name(&job.name, index);
    cmd.take_run(ct, name)
  });
  let mut js = JoinSet::new();
  for job in futures {
    js.spawn(job);
  }
  let w = Box::pin(js.join_all());

  select! {
    _ = tokio::signal::ctrl_c() => {
      println!("Ctrl-C received, cancelling jobs...");
      ct.cancel();
    }
    _ = w => {
      println!("All jobs completed");
    }
  }
  Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
  let args = Args::parse();
  let config = Config::from_file(&args.config_file)?;

  run_jobs(config.jobs).await?;
  Ok(())
}
