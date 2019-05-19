use crate::format::CodeStr;
use crossbeam::channel::{bounded, Sender};
use indicatif::{ProgressBar, ProgressStyle};
use scopeguard::guard;
use std::{
  fs::{create_dir_all, metadata, rename},
  io,
  io::{Read, Write},
  path::{Path, PathBuf},
  process::{ChildStdin, Command, Stdio},
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
  },
  thread,
  thread::sleep,
  time::{Duration, Instant},
};
use tempfile::tempdir;
use uuid::Uuid;
use walkdir::WalkDir;

// Construct a random image tag.
pub fn random_tag() -> String {
  Uuid::new_v4()
    .to_simple()
    .encode_lower(&mut Uuid::encode_buffer())
    .to_owned()
}

// Query whether an image exists locally.
pub fn image_exists(
  image: &str,
  running: &Arc<AtomicBool>,
) -> Result<bool, String> {
  debug!("Checking existence of image {}\u{2026}", image.code_str());
  if let Err(e) = run_quiet(
    "Checking existence of image...",
    "The image doesn't exist.",
    &["image", "inspect", image],
    running,
  ) {
    if running.load(Ordering::SeqCst) {
      Ok(false)
    } else {
      Err(e)
    }
  } else {
    Ok(true)
  }
}

// Push an image.
pub fn push_image(
  image: &str,
  running: &Arc<AtomicBool>,
) -> Result<(), String> {
  debug!("Pushing image {}\u{2026}", image.code_str());
  run_quiet(
    "Pushing image...",
    "Unable to push image.",
    &["image", "push", image],
    running,
  )
  .map(|_| ())
}

// Pull an image.
pub fn pull_image(
  image: &str,
  running: &Arc<AtomicBool>,
) -> Result<(), String> {
  debug!("Pulling image {}\u{2026}", image.code_str());
  run_quiet(
    "Pulling image...",
    "Unable to pull image.",
    &["image", "pull", image],
    running,
  )
  .map(|_| ())
}

// Delete an image.
pub fn delete_image(
  image: &str,
  running: &Arc<AtomicBool>,
) -> Result<(), String> {
  debug!("Deleting image {}\u{2026}", image.code_str());
  run_quiet(
    "Deleting image...",
    "Unable to delete image.",
    &["image", "rm", "--force", image],
    running,
  )
  .map(|_| ())
}

// Create a container and return its ID.
pub fn create_container(
  image: &str,
  running: &Arc<AtomicBool>,
) -> Result<String, String> {
  debug!("Creating container from image {}\u{2026}", image.code_str(),);

  // Why `--init`? (1) PID 1 is supposed to reap orphaned zombie processes,
  // otherwise they can accumulate. Bash does this, but we run `/bin/sh` in the
  // container, which may or may not be Bash. So `--init` runs Tini
  // (https://github.com/krallin/tini) as PID 1, which properly reaps orphaned
  // zombies. (2) PID 1 also does not exhibit the default behavior (crashing)
  // for signals like SIGINT and SIGTERM. However, PID 1 can still handle these
  // signals by explicitly trapping them. Tini traps these signals and forwards
  // them to the child process. Then the default signal handling behavior of
  // the child process (in our case, `/bin/sh`) works normally. [tag:--init]
  Ok(
    run_quiet(
      "Creating container...",
      "Unable to create container.",
      vec![
        "container",
        "create",
        "--init",
        "--interactive",
        image,
        "/bin/sh",
      ]
      .as_ref(),
      running,
    )?
    .trim()
    .to_owned(),
  )
}

// Copy files into a container.
pub fn copy_into_container<R: Read>(
  container: &str,
  mut tar: R,
  running: &Arc<AtomicBool>,
) -> Result<(), String> {
  debug!(
    "Copying files into container {}\u{2026}",
    container.code_str()
  );
  run_quiet_stdin(
    "Copying files into container...",
    "Unable to copy files into the container.",
    &["container", "cp", "-", &format!("{}:{}", container, "/")],
    |mut stdin| {
      io::copy(&mut tar, &mut stdin).map_err(|e| {
        format!("Unable to copy files into the container. Details: {}", e)
      })?;

      Ok(())
    },
    running,
  )
  .map(|_| ())
}

// Copy files from a container.
pub fn copy_from_container(
  container: &str,
  paths: &[PathBuf],
  source_dir: &Path,
  destination_dir: &Path,
  running: &Arc<AtomicBool>,
) -> Result<(), String> {
  // Copy each path from the container to the host.
  for path in paths {
    debug!(
      "Copying `{}` from container {}\u{2026}",
      path.to_string_lossy(),
      container.code_str()
    );

    // `docker container cp` is not idempotent. For example, suppose there is a
    // directory called `/foo` in the container and `/bar` does not exist on
    // the host. Consider the following command:
    //   `docker cp container:/foo /bar`
    // The first time that command is run, Docker will create the directory
    // `/bar` on the host and copy the files from `/foo` into it. But if you
    // run it again, Docker will copy `/bar` into the directory `/foo`,
    // resulting in `/foo/foo`, which is undesirable. To work around this, we
    // first copy the path from the container into a temporary directory (where
    // the target path is guaranteed to not exist). Then we copy/move that to
    // the final destination.
    let temp_dir = tempdir().map_err(|e| {
      format!("Unable to create temporary directory. Details: {}", e)
    })?;

    // Figure out what needs to go where.
    let source = source_dir.join(path);
    let intermediate = temp_dir.path().join("data");
    let destination = destination_dir.join(path);

    // Get the path from the container.
    run_quiet(
      "Copying files from the container...",
      "Unable to copy files from the container.",
      &[
        "container",
        "cp",
        &format!("{}:{}", container, source.to_string_lossy()),
        &intermediate.to_string_lossy(),
      ],
      running,
    )
    .map(|_| ())?;

    // Check if what we got from the container is a file or a directory.
    let metadata_err_map = |e| {
      format!(
        "Unable to retrieve filesystem metadata for path {}. Details: {}",
        intermediate.to_string_lossy().code_str(),
        e
      )
    };
    if metadata(&intermediate).map_err(metadata_err_map)?.is_file() {
      // It's a file. Determine the destination directory. The `unwrap` is safe
      // because the root of the filesystem cannot be a file.
      let destination_dir = destination.parent().unwrap().to_owned();

      // Make sure the destination directory exists.
      create_dir_all(&destination_dir).map_err(|e| {
        format!(
          "Unable to create directory {}. Details: {}",
          destination_dir.to_string_lossy().code_str(),
          e
        )
      })?;

      // Move it to the destination.
      rename(&intermediate, &destination).map_err(|e| {
        format!(
          "Unable to move file {} to destination {}. Details: {}",
          intermediate.to_string_lossy().code_str(),
          destination.to_string_lossy().code_str(),
          e
        )
      })?;
    } else {
      // It's a directory. Traverse it.
      for entry in WalkDir::new(&intermediate) {
        // If we run into an error traversing the filesystem, report it.
        let entry = entry.map_err(|e| {
          format!(
            "Unable to traverse directory {}. Details: {}",
            intermediate.to_string_lossy().code_str(),
            e
          )
        })?;

        // Figure out what needs to go where. The `unwrap` is safe because
        // `entry` is guaranteed to be inside `intermediate` (or equal to it).
        let entry_path = entry.path();
        let destination_path =
          destination.join(entry_path.strip_prefix(&intermediate).unwrap());

        // Check if the current entry is a file or a directory.
        if entry.file_type().is_dir() {
          // It's a directory. Create a directory at the destination.
          create_dir_all(&destination_path).map_err(|e| {
            format!(
              "Unable to create directory {}. Details: {}",
              destination_path.to_string_lossy().code_str(),
              e
            )
          })?;
        } else {
          // It's a file. Move it to the destination.
          rename(entry_path, &destination_path).map_err(|e| {
            format!(
              "Unable to move file {} to destination {}. Details: {}",
              entry_path.to_string_lossy().code_str(),
              destination_path.to_string_lossy().code_str(),
              e
            )
          })?;
        }
      }
    }
  }

  Ok(())
}

// Start a container.
pub fn start_container(
  container: &str,
  command: &str,
  running: &Arc<AtomicBool>,
) -> Result<(), String> {
  debug!("Starting container {}\u{2026}", container.code_str());
  run_loud_stdin(
    "Unable to start container.",
    &["container", "start", "--attach", "--interactive", container],
    |stdin| {
      write!(stdin, "{}", command).map_err(|e| {
        format!(
          "Unable to send command {} to the container. Details: {}",
          command.code_str(),
          e
        )
      })?;

      Ok(())
    },
    running,
  )
  .map(|_| ())
}

// Stop a container.
pub fn stop_container(
  container: &str,
  running: &Arc<AtomicBool>,
) -> Result<(), String> {
  debug!("Stopping container {}\u{2026}", container.code_str());
  run_quiet(
    "Stopping container...",
    "Unable to stop container.",
    &["container", "stop", container],
    running,
  )
  .map(|_| ())
}

// Commit a container to an image.
pub fn commit_container(
  container: &str,
  image: &str,
  running: &Arc<AtomicBool>,
) -> Result<(), String> {
  debug!(
    "Committing container {} to image {}\u{2026}",
    container.code_str(),
    image.code_str()
  );
  run_quiet(
    "Committing container...",
    "Unable to commit container.",
    &["container", "commit", container, image],
    running,
  )
  .map(|_| ())
}

// Delete a container.
pub fn delete_container(
  container: &str,
  running: &Arc<AtomicBool>,
) -> Result<(), String> {
  debug!("Deleting container {}\u{2026}", container.code_str());
  run_quiet(
    "Deleting container...",
    "Unable to delete container.",
    &["container", "rm", "--force", container],
    running,
  )
  .map(|_| ())
}

// Run an interactive shell.
pub fn spawn_shell(
  image: &str,
  running: &Arc<AtomicBool>,
) -> Result<(), String> {
  debug!(
    "Spawning an interactive shell for image {}\u{2026}",
    image.code_str()
  );
  run_attach(
    "The shell exited with a failure.",
    &[
      "container",
      "run",
      "--rm",
      "--interactive",
      "--tty",
      "--init", // [ref:--init]
      image,
      "/bin/su", // We use `su` rather than `sh` to use the root user's shell.
    ],
    running,
  )
}

// Run a command, forward its standard error stream, and return its standard
// output.
fn run_quiet(
  spinner_message: &str,
  error: &str,
  args: &[&str],
  running: &Arc<AtomicBool>,
) -> Result<String, String> {
  let _guard = spin(spinner_message);

  let output = command(args)
    .stdin(Stdio::null())
    .output()
    .map_err(|e| format!("{}\nDetails: {}", error, e))?;

  if output.status.success() {
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
  } else {
    Err(if output.status.code().is_none() {
      running.store(false, Ordering::SeqCst);
      super::INTERRUPT_MESSAGE.to_owned()
    } else {
      format!(
        "{}\nDetails: {}",
        error,
        String::from_utf8_lossy(&output.stderr)
      )
    })
  }
}

// Run a command, forward its standard error stream, and return its standard
// output. Accepts a closure which receives a pipe to the standard input stream
// of the child process.
fn run_quiet_stdin<W: FnOnce(&mut ChildStdin) -> Result<(), String>>(
  spinner_message: &str,
  error: &str,
  args: &[&str],
  writer: W,
  running: &Arc<AtomicBool>,
) -> Result<String, String> {
  let _guard = spin(spinner_message);

  let mut child = command(args)
    .stdin(Stdio::piped()) // [tag:run_quiet_stdin_piped]
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .map_err(|e| format!("{}\nDetails: {}", error, e))?;
  writer(child.stdin.as_mut().unwrap())?; // [ref:run_quiet_stdin_piped]
  let output = child
    .wait_with_output()
    .map_err(|e| format!("{}\nDetails: {}", error, e))?;

  if output.status.success() {
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
  } else {
    Err(if output.status.code().is_none() {
      running.store(false, Ordering::SeqCst);
      super::INTERRUPT_MESSAGE.to_owned()
    } else {
      format!(
        "{}\nDetails: {}",
        error,
        String::from_utf8_lossy(&output.stderr)
      )
    })
  }
}

// Run a command and forward its standard output and error streams. Accepts a
// closure which receives a pipe to the standard input stream of the child
// process.
fn run_loud_stdin<W: FnOnce(&mut ChildStdin) -> Result<(), String>>(
  error: &str,
  args: &[&str],
  writer: W,
  running: &Arc<AtomicBool>,
) -> Result<(), String> {
  let mut child = command(args)
    .stdin(Stdio::piped()) // [tag:run_loud_stdin_piped]
    .spawn()
    .map_err(|e| format!("{}\nDetails: {}", error, e))?;
  writer(child.stdin.as_mut().unwrap())?; // [ref:run_loud_stdin_piped]
  let status = child
    .wait()
    .map_err(|e| format!("{}\nDetails: {}", error, e))?;

  if status.success() {
    Ok(())
  } else {
    Err(
      if status.code().is_none() {
        running.store(false, Ordering::SeqCst);
        super::INTERRUPT_MESSAGE
      } else {
        error
      }
      .to_owned(),
    )
  }
}

// Run a command and forward its standard input, output, and error streams.
fn run_attach(
  error: &str,
  args: &[&str],
  running: &Arc<AtomicBool>,
) -> Result<(), String> {
  let status = command(args)
    .status()
    .map_err(|e| format!("{}\nDetails: {}", error, e))?;

  if status.success() {
    Ok(())
  } else {
    Err(
      if status.code().is_none() {
        running.store(false, Ordering::SeqCst);
        super::INTERRUPT_MESSAGE
      } else {
        error
      }
      .to_owned(),
    )
  }
}

// Construct a Docker `Command` from an array of arguments.
fn command(args: &[&str]) -> Command {
  let mut command = Command::new("docker");
  for arg in args {
    command.arg(arg);
  }
  command
}

// Render a spinner in the terminal. When the returned value is dropped, the
// spinner is stopped.
fn spin(message: &str) -> impl Drop {
  // Start a thread for our spinner-as-a-service. This thread will only be
  // created once and will live for the duration of the whole program.
  lazy_static! {
    static ref SPINNER_SERVICE: Sender<(String, Arc<AtomicBool>, Sender<()>)> = {
      // Create a channel for requests to start spinning.
      let (request_sender, request_receiver) =
        bounded::<(String, Arc<AtomicBool>, Sender<()>)>(0);

      // Start a thread to handle spinner requests.
      thread::spawn(move || loop {
        // Wait for a request. The `unwrap` is safe since we never hang up the
        // channel.
        let (message, spinning, response_sender) =
          request_receiver.recv().unwrap();

        // Create the spinner!
        let spinner = ProgressBar::new(1);
        spinner.set_style(ProgressStyle::default_spinner());
        spinner.set_message(&message);

        // Animate the spinner for as long as necessary.
        let now = Instant::now();
        while spinning.load(Ordering::SeqCst) {
          // Render the next frame of the spinner.
          spinner.tick();

          // For the first 100ms, we animate on a shorter time interval so we
          // can stop faster if the work finishes instantly. If the work takes
          // longer than that, we slow the animation down out of courtesy for
          // the CPU.
          if now.elapsed() < Duration::from_millis(100) {
            sleep(Duration::from_millis(16));
          } else {
            sleep(Duration::from_millis(100));
          }
        }

        // Clean up the spinner.
        spinner.finish_and_clear();

        // Inform the caller that the spinner has been cleaned up. The `unwrap`
        // is safe since we never hang up the channel.
        response_sender.send(()).unwrap();
      });

      // The sender half of the request channel is the API for this spinner
      // service. Return it.
      request_sender
    };
  }

  // Create a channel for waiting on the spinner.
  let (response_sender, response_receiver) = bounded::<()>(0);

  // This will be set to `false` when it's time to stop the spinner.
  let spinning = Arc::new(AtomicBool::new(true));

  // Create and animate the spinner. The `unwrap` is safe since we never hang
  // up the channel.
  SPINNER_SERVICE
    .send((message.to_owned(), spinning.clone(), response_sender))
    .unwrap();

  // Return a guard that stops the spinner via its destructor.
  guard((), move |_| {
    // Tell the spinner service to stop the spinner.
    spinning.store(false, Ordering::SeqCst);

    // Wait for the spinner to stop. The `unwrap` is safe since we never hang
    // up the channel.
    response_receiver.recv().unwrap();
  })
}

#[cfg(test)]
mod tests {
  use crate::docker::random_tag;

  #[test]
  fn random_impure() {
    assert_ne!(random_tag(), random_tag());
  }
}
