use std::io;
use std::process::{Child, Output};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) struct GateChild {
    child: Option<Child>,
}

impl GateChild {
    pub(crate) fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    pub(crate) fn wait_with_timeout(&mut self, timeout: Duration) -> io::Result<Output> {
        let deadline = Instant::now() + timeout;
        loop {
            let exited = self
                .child
                .as_mut()
                .expect("gate child already reaped")
                .try_wait()?
                .is_some();
            if exited {
                return self
                    .child
                    .take()
                    .expect("gate child already reaped")
                    .wait_with_output();
            }
            if Instant::now() >= deadline {
                let mut child = self.child.take().expect("gate child already reaped");
                let _ = child.kill();
                let output = child.wait_with_output()?;
                panic!(
                    "child did not exit within {timeout:?}; stdout={}, stderr={}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for GateChild {
    fn drop(&mut self) {
        if let Some(child) = self.child.take() {
            let _ = reap_owned_child(child);
        }
    }
}

pub(crate) fn reap_owned_child(mut child: Child) -> io::Result<()> {
    if wait_for_child_exit(&mut child, Duration::from_secs(1))? {
        child.wait_with_output()?;
        return Ok(());
    }

    let pid = child.id() as i32;
    // SAFETY: The child remains owned and unreaped here, so this PID cannot have been reused.
    unsafe {
        if libc::kill(pid, libc::SIGTERM) == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
    }
    if wait_for_child_exit(&mut child, Duration::from_millis(250))? {
        child.wait_with_output()?;
        return Ok(());
    }

    let _ = child.kill();
    child.wait_with_output()?;
    Ok(())
}

fn wait_for_child_exit(child: &mut Child, timeout: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(10));
    }
}
