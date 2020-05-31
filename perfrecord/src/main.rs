use profiler_symbol_server::start_server;
use serde_json::to_writer;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};
use structopt::StructOpt;
use which::which;

mod dyld_bindings;
mod gecko_profile;
mod proc_maps;
mod process_launcher;
mod task_profiler;
mod thread_profiler;

pub mod thread_act;
pub mod thread_info;

use process_launcher::{MachError, ProcessLauncher};
use task_profiler::TaskProfiler;

#[derive(Debug, StructOpt)]
#[structopt(
    name = "perfrecord",
    about = r#"Run a command and record a CPU profile of its execution.

EXAMPLES:
    perfrecord ./yourcommand args
    perfrecord --launch-when-done ./yourcommand args
    perfrecord -o prof.json ./yourcommand args
    perfrecord --launch prof.json"#
)]
struct Opt {
    /// Launch the profiler and display the collected profile after recording.
    #[structopt(long = "launch-when-done")]
    launch_when_done: bool,

    /// When recording, limit the recorded time to the specified number of seconds
    #[structopt(short = "t", long = "time-limit")]
    time_limit: Option<f64>,

    /// When recording, save the collected profile to this file.
    #[structopt(
        short = "o",
        long = "out",
        default_value = "profile.json",
        parse(from_os_str)
    )]
    output_file: PathBuf,

    /// If neither --launch nor --serve are specified, profile this command.
    #[structopt(subcommand)]
    rest: Option<Subcommands>,

    /// Launch the profiler in your default browser and display the selected profile.
    #[structopt(short = "l", long = "launch", parse(from_os_str))]
    file_to_launch: Option<PathBuf>,

    /// Serve the specified profile from a local webserver but do not open the browser.
    #[structopt(short = "s", long = "serve", parse(from_os_str))]
    file_to_serve: Option<PathBuf>,
}

#[derive(Debug, PartialEq, StructOpt)]
enum Subcommands {
    #[structopt(external_subcommand)]
    Command(Vec<String>),
}

fn main() -> Result<(), MachError> {
    let opt = Opt::from_args();
    let open_in_browser = opt.file_to_launch.is_some();
    let file_for_launching_or_serving = opt.file_to_launch.or(opt.file_to_serve);
    if let Some(file) = file_for_launching_or_serving {
        start_server_main(&file, open_in_browser);
        return Ok(());
    }
    if let Some(Subcommands::Command(command)) = opt.rest {
        if !command.is_empty() {
            start_recording(
                &opt.output_file,
                &command,
                opt.time_limit,
                opt.launch_when_done,
            )?;
            return Ok(());
        }
    }
    println!("Error: missing command\n");
    Opt::clap().print_help().unwrap();
    std::process::exit(1);
}

#[tokio::main]
async fn start_server_main(file: &Path, open_in_browser: bool) {
    start_server(file, open_in_browser).await;
}

fn sleep_and_save_overshoot(duration: Duration, overshoot: &mut Duration) {
    let before_sleep = Instant::now();
    thread::sleep(duration);
    let after_sleep = Instant::now();
    *overshoot = after_sleep
        .duration_since(before_sleep)
        .checked_sub(duration)
        .unwrap_or(Duration::from_nanos(0));
}

fn start_recording(
    output_file: &Path,
    args: &[String],
    time_limit: Option<f64>,
    launch_when_done: bool,
) -> Result<(), MachError> {
    let time_limit = time_limit.map(|secs| Duration::from_secs_f64(secs));

    let command_name = args.first().unwrap();
    let command = which(command_name).expect("Couldn't resolve command name");
    let args: Vec<&str> = args.iter().skip(1).map(std::ops::Deref::deref).collect();

    let mut launcher = ProcessLauncher::new(&command, &args)?;
    let child_pid = launcher.get_id();
    let child_task = launcher.take_task();
    println!("child PID: {}, childTask: {}\n", child_pid, child_task);

    let now = Instant::now();
    let sampling_start = now;
    let mut task_profiler = TaskProfiler::new(child_task, child_pid, now, command_name)
        .expect("couldn't create TaskProfiler");
    task_profiler.sample(now).expect("sampling failed");

    let interval = Duration::from_millis(1);

    launcher.start_execution();
    let mut last_sleep_overshoot = Duration::from_nanos(0);
    sleep_and_save_overshoot(interval, &mut last_sleep_overshoot);

    loop {
        let sample_timestamp = Instant::now();
        if let Some(time_limit) = time_limit {
            if sample_timestamp.duration_since(sampling_start) >= time_limit {
                break;
            }
        }
        let got_sample = task_profiler.sample(sample_timestamp).is_ok();
        if !got_sample {
            break;
        }
        let intended_wakeup_time = sample_timestamp + interval;
        let indended_wait_time = intended_wakeup_time.saturating_duration_since(Instant::now());
        let sleep_time = if indended_wait_time > last_sleep_overshoot {
            indended_wait_time - last_sleep_overshoot
        } else {
            Duration::from_nanos(0)
        };
        sleep_and_save_overshoot(sleep_time, &mut last_sleep_overshoot);
    }

    let profile_builder = task_profiler.into_profile();

    let file = File::create(output_file).unwrap();
    to_writer(file, &profile_builder.to_json()).expect("Couldn't write JSON");
    // println!("profile: {:?}", profile_builder);

    if launch_when_done {
        start_server_main(output_file, true);
    }

    let _exit_code = launcher.wait().expect("couldn't wait for child");

    Ok(())
}
