use std::{collections::HashMap, fs::File, io::BufReader, process, time::Duration};

use anyhow::Result;
use clap::{Parser, ValueEnum};
use colored::Colorize as _;
use serde::Deserialize;
use time::{OffsetDateTime, format_description::FormatItem};
use tokio::{
  io::{AsyncBufReadExt, BufReader as AsyncBufReader},
  process::{ChildStderr, ChildStdout, Command},
  select,
  task::JoinSet,
  time::{interval, sleep, timeout},
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
struct Job {
  cmd: Cmdline,
  workdir: Option<String>,
  env: Option<HashMap<String, String>>,
  inherit_env: Option<bool>,
  delay: Option<Delay>,
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

  fn from_args(args: &Args) -> Result<Self> {
    match args.config_format {
      ArgsConfigFormat::Json => Self::from_json(&args.config_file),
      ArgsConfigFormat::Yaml => Self::from_yaml(&args.config_file),
    }
  }
}

impl TryInto<Command> for Cmdline {
  type Error = anyhow::Error;

  fn try_into(self) -> Result<Command> {
    let cmdline = match self {
      Cmdline::Line(line) => line.split_ascii_whitespace().map(|s| s.to_string()).collect::<Vec<String>>(),
      Cmdline::Array(array) => array,
    };
    let cmd = cmdline.get(0).ok_or_else(|| anyhow::anyhow!("Command is empty"))?;
    let args = if cmdline.len() > 1 { Some(cmdline[1..].to_vec()) } else { None };
    let mut command = Command::new(cmd);
    if let Some(args) = args {
      command.args(args);
    }
    Ok(command)
  }
}

impl TryInto<Command> for &Cmdline {
  type Error = anyhow::Error;

  fn try_into(self) -> std::result::Result<Command, Self::Error> {
    let self2 = self.clone();
    self2.try_into()
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

impl TryInto<Command> for JobDefination {
  type Error = anyhow::Error;

  fn try_into(self) -> Result<Command> {
    match self {
      JobDefination::Cmdline(cmdline) => {
        let job = Job {
          cmd: cmdline,
          workdir: None,
          env: None,
          inherit_env: None,
          delay: None,
        };
        job.try_into()
      }
      JobDefination::Job(job) => job.try_into(),
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
  async fn run(self, ct: CancellationToken, name: &String) -> Result<()>;
}

impl CommandExt for Command {
  async fn run(mut self, ct: CancellationToken, name: &String) -> Result<()> {
    print(name.as_str(), format!("Process start"), true);
    let mut child = self.spawn()?;
    let stdout = child.stdout.take().ok()?;
    let stderr = child.stderr.take().ok()?;

    let mut stdout_bufreader = AsyncBufReader::new(stdout);
    let mut stderr_bufreader = AsyncBufReader::new(stderr);

    let r = loop {
      select! {
        _ = ct.cancelled() => {
          child.kill().await?;
          break Err(anyhow::anyhow!("Cancelled"));
        }
        _ = stdout_bufreader.reprint(&name) => (),
        _ = stderr_bufreader.reprint(&name) => (),
        status = child.wait() => {
          let code = status?.code().ok()?;
          print(name.as_str(), format!("Process exited with code: {code}"), true);
          break Ok(());
        }
      }
    };
    r
  }
}

async fn run_jobs(jobs: HashMap<String, JobDefination>) -> Result<()> {
  let ct = CancellationToken::new();
  let mut js: JoinSet<()> = JoinSet::new();
  for (index, (name, job)) in jobs.iter().enumerate() {
    let ct = ct.clone();
    let Ok(cmd): Result<Command> = job.clone().try_into() else {
      eprintln!("Failed to convert job to command: {name}");
      continue;
    };
    let name = colored_name(name, index);
    let job = job.clone();
    js.spawn(async move {
      if let JobDefination::Job(job) = job {
        if let Some(delay) = job.delay {
          match delay {
            Delay::ConstantDuration(duration) => {
              print(name.as_str(), format!("Sleeping for {duration}ms"), true);
              sleep(Duration::from_millis(duration)).await;
            }
            Delay::Command(command) => {
              let mut interval = interval(Duration::from_millis(command.interval));
              loop {
                interval.tick().await;
                let Ok(mut command): Result<Command> = (&command.cmd).try_into() else {
                  eprintln!("Failed to parse dependency cmdline: {name}");
                  return;
                };
                let success = timeout(Duration::from_millis(10000), async {
                  if let Ok(status) = command.status().await {
                    return status.success();
                  }
                  false
                })
                .await;
                if let Ok(true) = success {
                  break;
                }
              }
            }
          }
        }
      }
      if let Err(e) = cmd.run(ct, &name).await {
        eprintln!("Failed to run job: {name}, error: {e}");
      }
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
  let config = Config::from_args(&args)?;

  run_jobs(config.jobs).await?;
  Ok(())
}
