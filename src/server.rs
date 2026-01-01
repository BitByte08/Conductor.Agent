use tokio::process::{Command, Child, ChildStdin};
use tokio::io::{AsyncBufReadExt, BufReader, AsyncWriteExt};
use std::process::Stdio;
use tokio::sync::mpsc;
use log::{info, error, warn};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};

pub struct ServerProcess {
    child: Option<Arc<Mutex<Child>>>,
    stdin: Option<Arc<Mutex<ChildStdin>>>,
    pid: Option<u32>,
}

#[derive(Debug)]
pub enum ServerEvent {
    Output(String), // stdout/stderr line
    Exit(Option<i32>),
}

impl ServerProcess {
    pub fn new() -> Self {
        Self { child: None, stdin: None, pid: None }
    }

    fn pid_file_path() -> String {
        "minecraft/server.pid".to_string()
    }

    async fn write_pid(&self) -> anyhow::Result<()> {
        if let Some(pid) = self.pid {
            tokio::fs::write(Self::pid_file_path(), pid.to_string()).await?;
            info!("Wrote PID {} to {}", pid, Self::pid_file_path());
        }
        Ok(())
    }

    async fn remove_pid_file() -> anyhow::Result<()> {
        let path = Self::pid_file_path();
        if tokio::fs::metadata(&path).await.is_ok() {
            tokio::fs::remove_file(&path).await?;
            info!("Removed PID file: {}", path);
        }
        Ok(())
    }

    pub async fn try_recover(&mut self) -> anyhow::Result<()> {
        let pid_path = Self::pid_file_path();
        if let Ok(pid_str) = tokio::fs::read_to_string(&pid_path).await {
            if let Ok(pid) = pid_str.trim().parse::<i32>() {
                // Check if process exists
                if let Ok(status) = std::process::Command::new("kill").arg("-0").arg(pid.to_string()).status() {
                    if status.success() {
                        info!("Recovered existing Minecraft server with PID {}", pid);
                        self.pid = Some(pid as u32);
                        return Ok(());
                    }
                }
                warn!("PID {} in file but process not found, removing stale PID file", pid);
                Self::remove_pid_file().await?;
            }
        }
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        // Check both child handle and recovered PID
        self.child.is_some() || self.pid.is_some()
    }

    pub async fn start(&mut self, jar_path: &str, args: Vec<String>, event_tx: mpsc::Sender<ServerEvent>) -> anyhow::Result<()> {
        if self.is_running() {
            return Err(anyhow::anyhow!("Server is already running"));
        }

        info!("Starting server: java -jar {} {:?}", jar_path, args);

        // Parse JVM args and program args clearly
        // Expected format: java [JVM Args] -jar server.jar [Program Args]
        // The `args` valid here are MIXED, which is bad. The caller should separate them.
        // For now, let's assume `args` passed from main.rs are ONLY JVM args + "nogui".
        // BUT wait, in main.rs: args = ["-Xmx...", "-Xms...", "nogui"]
        // "nogui" is a PROGRAM arg. -Xmx is a JVM arg.
        
        let mut jvm_args = Vec::new();
        let mut prog_args = Vec::new();

        for arg in args {
            if arg.starts_with("-X") || arg.starts_with("-D") {
                jvm_args.push(arg);
            } else {
                prog_args.push(arg);
            }
        }

        info!("Starting server with JVM: {:?} and Program: {:?}", jvm_args, prog_args);

        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        // If jar_path is an absolute path, run in its parent directory so the server picks up eula.txt, world/, etc.
        let jar_path_buf = std::path::PathBuf::from(jar_path);
        let cwd = jar_path_buf.parent().map(|p| p.to_path_buf()).unwrap_or(std::path::PathBuf::from(home).join("conductor"));

        let mut child = Command::new("java")
            .args(jvm_args)
            .arg("-jar")
            .arg(jar_path)
            .args(prog_args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()) // Capture stderr too
            .spawn()?;

        let stdin = child.stdin.take().ok_or(anyhow::anyhow!("Failed to capture stdin"))?;
        let stdout = child.stdout.take().ok_or(anyhow::anyhow!("Failed to capture stdout"))?;
        let stderr = child.stderr.take().ok_or(anyhow::anyhow!("Failed to capture stderr"))?;

        // Spawn capture task for stdout
        let tx = event_tx.clone();
        let stdout_task = tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
               if let Err(_) = tx.send(ServerEvent::Output(line)).await { break; }
            }
        });

        // Spawn capture task for stderr
        let tx_err = event_tx.clone();
        let stderr_task = tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
               if let Err(_) = tx_err.send(ServerEvent::Output(line)).await { break; }
            }
        });

        // Wrap child and stdin in Arc<Mutex> so we can await/kill from other tasks
        let child_arc = Arc::new(Mutex::new(child));
        let stdin_arc = Arc::new(Mutex::new(stdin));

        // Spawn waiter to detect exit
        let tx_wait = event_tx.clone();
        let child_clone = child_arc.clone();
        tokio::spawn(async move {
            let mut guard = child_clone.lock().await;
            match guard.wait().await {
                Ok(status) => {
                    let code = status.code();
                    let _ = tx_wait.send(ServerEvent::Output(format!("Server exited with status: {:?}", code))).await;
                    let _ = tx_wait.send(ServerEvent::Exit(code)).await;
                }
                Err(e) => {
                    let _ = tx_wait.send(ServerEvent::Output(format!("Server wait error: {}", e))).await;
                    let _ = tx_wait.send(ServerEvent::Exit(None)).await;
                }
            }
        });

        // Get PID before moving child
        let pid = {
            let guard = child_arc.lock().await;
            guard.id()
        };

        self.child = Some(child_arc);
        self.stdin = Some(stdin_arc);
        self.pid = pid;
        self.write_pid().await?;

        Ok(())
    }

    pub async fn graceful_stop(&mut self) -> anyhow::Result<()> {
        if self.child.is_none() && self.pid.is_some() {
            // Recovered process - kill by PID
            info!("Killing recovered server process PID: {}", self.pid.unwrap());
            let _ = std::process::Command::new("kill").arg(self.pid.unwrap().to_string()).status();
            tokio::time::sleep(Duration::from_secs(3)).await;
            let _ = std::process::Command::new("kill").arg("-9").arg(self.pid.unwrap().to_string()).status();
            self.pid = None;
            Self::remove_pid_file().await?;
            return Ok(());
        }

        if let Some(stdin_arc) = &self.stdin {
            info!("Sending 'stop' command to server...");
            let mut guard = stdin_arc.lock().await;
            let _ = guard.write_all(b"stop\n").await;
            let _ = guard.flush().await;
            drop(guard);
            
            // Wait up to 10 seconds for graceful shutdown
            for i in 0..10 {
                tokio::time::sleep(Duration::from_secs(1)).await;
                // Check if both child and pid are cleared
                if !self.is_running() {
                    info!("Server stopped gracefully after {} seconds", i + 1);
                    Self::remove_pid_file().await?;
                    return Ok(());
                }
            }
            warn!("Server did not stop gracefully, forcing kill...");
        }
        
        self.stop().await
    }
    
    pub async fn cleanup_on_exit(&mut self) {
        // Called when ServerEvent::Exit is received
        self.child = None;
        self.stdin = None;
        self.pid = None;
        let _ = Self::remove_pid_file().await;
        info!("Server process cleaned up after exit");
    }

    pub async fn stop(&mut self) -> anyhow::Result<()> {
        if let Some(child_arc) = self.child.take() {
            info!("Stopping server...");
            let mut guard = child_arc.lock().await;
            guard.kill().await?;
            self.stdin = None;
            self.pid = None;
            Self::remove_pid_file().await?;
            info!("Server stopped.");
        }
        Ok(())
    }

    pub async fn write_command(&mut self, cmd: &str) -> anyhow::Result<()> {
        if let Some(stdin_arc) = &self.stdin {
            let mut guard = stdin_arc.lock().await;
            guard.write_all(format!("{}\n", cmd).as_bytes()).await?;
            guard.flush().await?;
            Ok(())
        } else {
            Err(anyhow::anyhow!("Server not running (no stdin)"))
        }
    }
}
