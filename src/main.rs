use std::{collections::HashMap, fs::File, io::BufReader, process, time::Duration};

use anyhow::Result;
use clap::{Parser, ValueEnum};
use colored::Colorize as _;
use serde::Deserialize;
use time::{OffsetDateTime, format_description::FormatItem};
use tokio::{
  io::{AsyncBufReadExt, BufReader as AsyncBufReader},
  process::{Child, ChildStderr, ChildStdout, Command},
  select,
  task::JoinSet,
  time::{interval, sleep},
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

fn print_to_console(name: &str, line: &str, is_err: bool) {
  let Ok(Ok(timestamp)) = OffsetDateTime::now_local().map(|t| t.format(TIMESTAMP_FORMAT_OFFSET)) else {
    eprintln!("Failed to get local time");
    return;
  };

  let timestamp = format!("{timestamp:<29}").bright_black().to_string();
  let line = line.trim();
  let msg = format!("[{timestamp}] {name}:: {line}");

  if is_err {
    eprintln!("{msg}");
  } else {
    println!("{msg}");
  }
}

// macro_rules! format_to_console {
//   ($name:expr, $fmt:literal $(, $args:expr)*) => {
//     print_to_console($name, &format!($fmt $(, $args)*), false)
//   };
// }

macro_rules! format_to_console_err {
  ($name:expr, $fmt:literal $(, $args:expr)*) => {
    print_to_console($name, &format!($fmt $(, $args)*), true)
  };
}

#[derive(Debug, Clone, ValueEnum)]
#[clap(rename_all = "kebab-case")]
enum ArgsConfigFormat {
  Json,
  Yaml,
}

#[derive(Parser, Debug)]
struct Args {
  #[clap(short = 'c', long, env = "PA_CONFIG", default_value = "pa.json")]
  config_file: String,

  #[clap(short = 'f', long, env = "PA_FORMAT", default_value = "json")]
  config_format: ArgsConfigFormat,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum Cmdline {
  Line(String),
  Array(Vec<String>),
}

#[derive(Debug, Deserialize, Clone)]
struct TestCommand {
  interval: u64,
  cmd: Cmdline,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum Delay {
  ConstantDuration(u64),
  Command(TestCommand),
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "kebab-case")]
enum Restart {
  Always,
  Never,
  OnFailure,
}

#[derive(Debug, Deserialize, Clone)]
struct Job {
  cmd: Cmdline,
  workdir: Option<String>,
  env: Option<HashMap<String, String>>,
  inherit_env: Option<bool>,
  delay: Option<Delay>,
  restart: Option<Restart>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum JobDefination {
  Cmdline(Cmdline),
  Job(Job),
}

#[derive(Debug, Deserialize, Clone)]
struct Config {
  jobs: HashMap<String, JobDefination>,
}

impl Config {
  fn from_json(file: &str) -> Result<Self> {
    let file = File::open(file)?;
    let reader = BufReader::new(file);
    let config: Config = serde_json::from_reader(reader)?;
    Ok(config)
  }

  fn from_yaml(file: &str) -> Result<Self> {
    let file = File::open(file)?;
    let reader = BufReader::new(file);
    let config: Config = serde_yml::from_reader(reader)?;
    Ok(config)
  }
}

impl TryInto<Config> for Args {
  type Error = anyhow::Error;

  fn try_into(self) -> Result<Config> {
    let config = match self.config_format {
      ArgsConfigFormat::Json => Config::from_json(&self.config_file),
      ArgsConfigFormat::Yaml => Config::from_yaml(&self.config_file),
    }?;
    Ok(config)
  }
}

impl TryInto<Command> for Cmdline {
  type Error = anyhow::Error;

  fn try_into(self) -> Result<Command> {
    let cmdline = match self {
      Cmdline::Line(line) => line.split_ascii_whitespace().map(|s| s.to_string()).collect::<Vec<String>>(),
      Cmdline::Array(array) => array,
    };
    let cmd = cmdline.first().ok_or_else(|| anyhow::anyhow!("Command is empty"))?;
    let args = if cmdline.len() > 1 { Some(cmdline[1..].to_vec()) } else { None };
    let mut command = Command::new(cmd);
    if let Some(args) = args {
      command.args(args);
    }
    command.kill_on_drop(true);
    Ok(command)
  }
}

impl TryInto<Command> for Job {
  type Error = anyhow::Error;

  fn try_into(self) -> Result<Command> {
    let mut command: Command = self.cmd.try_into()?;
    if let Some(workdir) = self.workdir {
      command.current_dir(workdir);
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
    Ok(command)
  }
}

impl From<Option<Restart>> for Restart {
  fn from(restart: Option<Restart>) -> Self {
    match restart {
      Some(restart) => restart,
      None => Restart::Never,
    }
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

trait ChildStdoutExt {
  async fn reprint(&mut self, name: &str) -> Result<()>;
}

impl ChildStdoutExt for AsyncBufReader<ChildStdout> {
  async fn reprint(&mut self, name: &str) -> Result<()> {
    let mut buf = String::new();
    self.read_line(&mut buf).await?;
    if !buf.is_empty() {
      print_to_console(name, buf.as_str(), false);
    }
    Ok(())
  }
}

impl ChildStdoutExt for AsyncBufReader<ChildStderr> {
  async fn reprint(&mut self, name: &str) -> Result<()> {
    let mut buf = String::new();
    self.read_line(&mut buf).await?;
    if !buf.is_empty() {
      print_to_console(name, buf.as_str(), true);
    }
    Ok(())
  }
}

trait ChildExt {
  async fn exit_ok(&mut self, name: &str) -> Result<bool>;
  async fn stop(&mut self, name: &str) -> Result<bool>;
  fn take_output(&mut self) -> Result<(AsyncBufReader<ChildStdout>, AsyncBufReader<ChildStderr>)>;
}

impl ChildExt for Child {
  async fn exit_ok(&mut self, name: &str) -> Result<bool> {
    let status = self.wait().await?;
    let code = status.code().ok()?;
    format_to_console_err!(name, "Process exited with code: {code}");
    Ok(status.success())
  }

  async fn stop(&mut self, _: &str) -> Result<bool> {
    #[cfg(unix)]
    {
      if let Some(pid) = self.id() {
        let pid = pid as i32;
        if let Err(e) = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGTERM) {
          eprintln!("Failed to send SIGTERM to process {pid}: {e}");
        } else {
          let _ = tokio::time::timeout(Duration::from_secs(5), self.wait()).await;
        }
      } else {
        eprintln!("Failed to get process ID");
      }
    }

    self.kill().await?;
    Err(anyhow::anyhow!("Cancelled"))
  }

  fn take_output(&mut self) -> Result<(AsyncBufReader<ChildStdout>, AsyncBufReader<ChildStderr>)> {
    let stdout = self.stdout.take().ok()?;
    let stderr = self.stderr.take().ok()?;

    let stdout_bufreader = AsyncBufReader::new(stdout);
    let stderr_bufreader = AsyncBufReader::new(stderr);

    Ok((stdout_bufreader, stderr_bufreader))
  }
}

trait CommandExt: Sized {
  async fn run(self, ct: CancellationToken, name: &str) -> Result<bool>;
  async fn success(self, timeout: u64) -> Result<bool>;
}

impl CommandExt for Command {
  async fn run(mut self, ct: CancellationToken, name: &str) -> Result<bool> {
    print_to_console(name, "Process start", true);
    let mut child = self.spawn()?;

    let (mut stdout, mut stderr) = match child.take_output() {
      Ok(v) => v,
      Err(e) => {
        format_to_console_err!(name, "Failed to take output, error: {e}");
        child.kill().await?;
        return Err(e);
      }
    };

    loop {
      select! {
        _ = ct.cancelled() => {
          break child.stop(name).await
        }
        _ = stdout.reprint(name) => (),
        _ = stderr.reprint(name) => (),
        success = child.exit_ok(name) => {
          break Ok(success?)
        }
      }
    }
  }

  async fn success(mut self, timeout: u64) -> Result<bool> {
    let success = tokio::time::timeout(Duration::from_millis(timeout), async { self.status().await }).await;
    Ok(success??.success())
  }
}

impl CommandExt for &Cmdline {
  async fn run(self, ct: CancellationToken, name: &str) -> Result<bool> {
    let self2 = self.clone();
    let command: Command = self2.try_into()?;
    command.run(ct, name).await
  }

  async fn success(self, timeout: u64) -> Result<bool> {
    let self2 = self.clone();
    let command: Command = self2.try_into()?;
    command.success(timeout).await
  }
}

impl CommandExt for &Job {
  async fn run(self, ct: CancellationToken, name: &str) -> Result<bool> {
    let self2 = self.clone();
    let command: Command = self2.try_into()?;
    command.run(ct, name).await
  }

  async fn success(self, timeout: u64) -> Result<bool> {
    let self2 = self.clone();
    let command: Command = self2.try_into()?;
    command.success(timeout).await
  }
}

impl CommandExt for &JobDefination {
  async fn run(self, ct: CancellationToken, name: &str) -> Result<bool> {
    match self {
      JobDefination::Cmdline(cmdline) => cmdline.run(ct, name).await,
      JobDefination::Job(job) => job.run(ct, name).await,
    }
  }

  async fn success(self, timeout: u64) -> Result<bool> {
    match self {
      JobDefination::Cmdline(cmdline) => cmdline.success(timeout).await,
      JobDefination::Job(job) => job.success(timeout).await,
    }
  }
}

trait JobExt {
  async fn delay(self, name: &str) -> Result<()>;
  async fn run_with_retry(self, ct: CancellationToken, name: &str) -> Result<bool>;
}

impl JobExt for &Cmdline {
  async fn delay(self, _: &str) -> Result<()> { Ok(()) }

  async fn run_with_retry(self, ct: CancellationToken, name: &str) -> Result<bool> {
    let job = Job {
      cmd: self.clone(),
      workdir: None,
      env: None,
      inherit_env: None,
      delay: None,
      restart: None,
    };
    let command: Command = job.try_into()?;
    command.run(ct, name).await
  }
}

impl JobExt for &Job {
  async fn delay(self, name: &str) -> Result<()> {
    if let Some(delay) = &self.delay {
      match delay {
        Delay::ConstantDuration(duration) => {
          format_to_console_err!(name, "Sleeping for {duration}ms");
          sleep(Duration::from_millis(*duration)).await;
        }
        Delay::Command(command) => {
          let mut interval = interval(Duration::from_millis(command.interval));
          loop {
            interval.tick().await;
            if let Ok(true) = command.cmd.success(10000).await {
              break;
            }
          }
        }
      }
    }
    Ok(())
  }

  async fn run_with_retry(self, ct: CancellationToken, name: &str) -> Result<bool> {
    let restart = Restart::from(self.restart.clone());
    loop {
      let success = match self.run(ct.clone(), name).await {
        Ok(true) => {
          println!("Job {name} completed successfully");
          true
        }
        Ok(false) => {
          print_to_console(name, "Job failed", true);
          false
        }
        Err(e) => {
          format_to_console_err!(name, "Job failed, error: {e}");
          false
        }
      };
      match restart {
        Restart::Always => {
          continue;
        }
        Restart::Never => break,
        Restart::OnFailure => {
          if success {
            break;
          } else {
            continue;
          }
        }
      }
    }
    Ok(true)
  }
}

impl JobExt for &JobDefination {
  async fn delay(self, name: &str) -> Result<()> {
    match self {
      JobDefination::Cmdline(cmdline) => cmdline.delay(name).await,
      JobDefination::Job(job) => job.delay(name).await,
    }
  }

  async fn run_with_retry(self, ct: CancellationToken, name: &str) -> Result<bool> {
    match self {
      JobDefination::Cmdline(cmdline) => cmdline.run_with_retry(ct, name).await,
      JobDefination::Job(job) => job.run_with_retry(ct, name).await,
    }
  }
}

impl JobDefination {
  async fn run(self, ct: CancellationToken, name: &String) {
    if let Err(e) = self.delay(name).await {
      format_to_console_err!(name, "Failed to delay job, error: {e}");
      return;
    }
    if let Err(e) = self.run_with_retry(ct, name).await {
      format_to_console_err!(name, "Failed to run job, error: {e}");
    }
  }
}

async fn run_jobs<Jobs>(jobs: Jobs) -> Result<()>
where
  Jobs: IntoIterator<Item = (String, JobDefination)>,
  Jobs::IntoIter: Send,
  Jobs::Item: Send,
{
  let ct = CancellationToken::new();
  let mut js: JoinSet<()> = JoinSet::new();
  for (index, (name, job)) in jobs.into_iter().enumerate() {
    let ct = ct.clone();
    let name = colored_name(name.as_str(), index);
    let job = job.clone();
    js.spawn(async move {
      job.run(ct, &name).await;
    });
  }
  let mut w = Box::pin(js.join_all());

  select! {
    _ = tokio::signal::ctrl_c() => {
      println!("Ctrl-C received, cancelling jobs...");
      ct.cancel();
      w.await;
    }
    _ = &mut w => {
      println!("All jobs completed");
    }
  }
  Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
  let args = Args::parse();
  let config: Config = args.try_into()?;

  run_jobs(config.jobs).await?;
  Ok(())
}
