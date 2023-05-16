use crate::helper::DynError;
use nix::{
    libc,
    sys::{
        signal::{killpg, signal, SigHandler, Signal},
        wait::{waitpid, WaitPidFlag, WaitStatus},
    },
    unistd::{self, dup2, execvp, fork, pipe, setpgid, tcgetpgrp, tcsetpgrp, ForkResult, Pid},
};
use rustyline::{error::ReadlineError, Editor};
use signal_hook::{consts::*, iterator::Signals};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env::set_current_dir,
    ffi::CString,
    mem::replace,
    path::PathBuf,
    process::exit,
    sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender},
    thread,
};

/// for freeing resources of syscall
struct CleanUp<F: Fn()> {
    f: F,
}

impl<F> Drop for CleanUp<F>
where
    F: Fn(),
{
    fn drop(&mut self) {
        (self.f)()
    }
}

/// wrapper of syscall
/// if syscall returns EINTR, then retry
fn syscall<F, T>(f: F) -> Result<T, nix::Error>
where
    F: Fn() -> Result<T, nix::Error>,
{
    loop {
        match f() {
            Err(nix::Error::EINTR) => (),
            result => return result,
        }
    }
}

/// message that worker thread receive
enum WorkerMsg {
    Signal(i32), // receive message
    Cmd(String), // command input
}

/// message that main thread receive
enum ShellMsg {
    Continue(i32), // restart reading shell, i32 is exit code
    Quit(i32),     // terminate shell, i32 is exit code of shell
}

#[derive(Debug)]
pub struct Shell {
    logfile: String,
}

impl Shell {
    pub fn new(logfile: &str) -> Self {
        Shell {
            logfile: logfile.to_string(),
        }
    }

    /// main thread
    pub fn run(&self) -> Result<(), DynError> {
        // if SITTOU is not set to ignore, SIGTSTP will be delivered.
        unsafe { signal(Signal::SIGTTOU, SigHandler::SigIgn).unwrap() };

        // at first, generate editor of rustline and load history file
        let mut rl = Editor::<()>::new()?;
        if let Err(e) = rl.load_history(&self.logfile) {
            eprintln!("ZeroSh: fail to read history file: {e}");
        }

        // generate channel and spawn signal_handler thread and worker thread
        let (worker_tx, worker_rx) = channel();
        let (shell_tx, shell_rx) = sync_channel(0);
        spawn_sig_handler(worker_tx.clone())?;
        Worker::new().spawn(worker_rx, shell_tx);

        let exit_val;
        let mut prev_exit_val = 0;
        loop {
            // read a line, and send it to worker thread
            let face = if prev_exit_val == 0 {
                '\u{1F642}' // 🙂
            } else {
                '\u{1F480}' // 💀
            };
            match rl.readline(&format!("ZeroSH {face} %> ")) {
                Ok(line) => {
                    let line_trimed = line.trim(); // trim space
                    if line_trimed.is_empty() {
                        continue; // command is empty
                    } else {
                        rl.add_history_entry(line_trimed);
                    }
                    worker_tx.send(WorkerMsg::Cmd(line)).unwrap();
                    match shell_rx.recv().unwrap() {
                        ShellMsg::Continue(n) => prev_exit_val = n,
                        ShellMsg::Quit(n) => {
                            exit_val = n;
                            break;
                        }
                    }
                }
                // ctrl + c
                Err(ReadlineError::Interrupted) => eprintln!("ZeroSh: Ctrl+d to end"),
                // ctrl + d
                Err(ReadlineError::Eof) => {
                    worker_tx.send(WorkerMsg::Cmd("exit".to_string())).unwrap();
                    match shell_rx.recv().unwrap() {
                        ShellMsg::Quit(n) => {
                            exit_val = n;
                            break;
                        }
                        _ => panic!("failed to exit"),
                    }
                }
                Err(e) => {
                    eprintln!("ZeroSh: failed to read\n{e}");
                    exit_val = 1;
                    break;
                }
            }
        }

        if let Err(e) = rl.save_history(&self.logfile) {
            eprintln!("ZeroSh: failed to write history file : {e}");
        }
        exit(exit_val);
    }
}

/// signal_handler thread
fn spawn_sig_handler(tx: Sender<WorkerMsg>) -> Result<(), DynError> {
    let mut signals = Signals::new(&[SIGINT, SIGTSTP, SIGCHLD])?;
    thread::spawn(move || {
        for sig in signals.forever() {
            tx.send(WorkerMsg::Signal(sig)).unwrap();
        }
    });

    Ok(())
}

#[derive(Debug, PartialEq, Eq, Clone)]
enum ProcState {
    Run,  // in progress
    Stop, // not in progress
}

#[derive(Debug, Clone)]
struct ProcInfo {
    state: ProcState,
    pgid: Pid, // process group id
}

type JobId = usize;

#[derive(Debug)]
struct Worker {
    exit_val: i32,   // exit code
    fg: Option<Pid>, // pid of foreground

    // jobid -> (process group id, command)
    jobs: BTreeMap<JobId, (Pid, String)>,

    pgid_to_pids: HashMap<Pid, (JobId, HashSet<Pid>)>,

    pid_to_info: HashMap<Pid, ProcInfo>,
    shell_pgid: Pid, // pid of shell
}

impl Worker {
    fn new() -> Self {
        Worker {
            exit_val: 0,
            fg: None, // foreground process is shell
            jobs: BTreeMap::new(),
            pgid_to_pids: HashMap::new(),
            pid_to_info: HashMap::new(),

            // get Pid of shell
            shell_pgid: tcgetpgrp(libc::STDIN_FILENO).unwrap(),
        }
    }

    /// activate worker thread
    fn spawn(mut self, worker_rx: Receiver<WorkerMsg>, shell_tx: SyncSender<ShellMsg>) {
        thread::spawn(move || {
            for msg in worker_rx.iter() {
                match msg {
                    WorkerMsg::Cmd(line) => match parse_cmd(&line) {
                        Ok(cmd) => {
                            if self.built_in_cmd(&cmd, &shell_tx) {
                                continue;
                            }

                            todo!();
                        }
                        Err(e) => {
                            eprintln!("ZeroSh: {}", e);
                            shell_tx.send(ShellMsg::Continue(self.exit_val)).unwrap();
                        }
                    },
                    WorkerMsg::Signal(SIGCHLD) => {
                        // self.wait_child(&shell_tx);
                        unimplemented!()
                    }
                    _ => (),
                }
            }
        });
    }

    /// return true if built in command
    fn built_in_cmd(&mut self, cmd: &[(&str, Vec<&str>)], shell_tx: &SyncSender<ShellMsg>) -> bool {
        if cmd.len() > 1 {
            return false;
        }
        match cmd[0].0 {
            "exit" => self.run_exit(&cmd[0].1, shell_tx),
            "jobs" => self.run_jobs(shell_tx),
            "fg" => self.run_fg(&cmd[0].1, shell_tx),
            "cd" => self.run_cd(&cmd[0].1, shell_tx),
            _ => false,
        }
    }

    /// run exit command
    ///
    /// if first argument is specified, exit the shell with it as the exit code
    /// if no argument is given, the shell exits with the last exit code of the last exited value
    fn run_exit(&mut self, args: &[&str], shell_tx: &SyncSender<ShellMsg>) -> bool {
        if !self.jobs.is_empty() {
            eprintln!("can't exit because job is running!");
            self.exit_val = 1;
            shell_tx.send(ShellMsg::Continue(self.exit_val)).unwrap();
            return true;
        }

        let exit_val = if let Some(s) = args.get(1) {
            if let Ok(n) = s.parse::<i32>() {
                n
            } else {
                eprintln!("{s} is invalid argment!");
                self.exit_val = 1;
                shell_tx.send(ShellMsg::Continue(self.exit_val)).unwrap();
                return true;
            }
        } else {
            self.exit_val
        };

        shell_tx.send(ShellMsg::Quit(exit_val)).unwrap();
        true
    }

    /// jobID PId State Jobs
    fn run_jobs(&mut self, shell_tx: &SyncSender<ShellMsg>) -> bool {
        for (job_id, (pgid, cmd)) in &self.jobs {
            let is_group_stop = self
                .pgid_to_pids
                .get(&pgid)
                .unwrap()
                .1
                .iter()
                .all(|pid| self.pid_to_info.get(pid).unwrap().state == ProcState::Stop);
            let state_str = if is_group_stop { "Running" } else { "Stopped" };
            println!("[{job_id}] {state_str}\t{cmd}");
        }
        self.exit_val = 0;
        shell_tx.send(ShellMsg::Continue(self.exit_val)).unwrap();

        true
    }

    /// brings jobs running in the background to the foreground
    fn run_fg(&mut self, args: &[&str], shell_tx: &SyncSender<ShellMsg>) -> bool {
        self.exit_val = 1; // temporary value
        if args.len() < 2 {
            eprintln!("usage: fg <number>");
            shell_tx.send(ShellMsg::Continue(self.exit_val)).unwrap();
            return true;
        }

        if let Ok(job_id) = args[1].parse::<usize>() {
            if let Some((pgid, cmd)) = self.jobs.get(&job_id) {
                eprintln!("[{job_id}] restart\t{cmd}");

                // set as foreground
                self.fg = Some(*pgid);
                tcsetpgrp(libc::STDIN_FILENO, *pgid).unwrap();

                // restart the job
                killpg(*pgid, Signal::SIGCONT).unwrap();
                return true;
            }
        }

        // fail
        eprintln!("job id [{}] was not found", args[1]);
        shell_tx.send(ShellMsg::Continue(self.exit_val)).unwrap();

        true
    }

    /// change current directiory
    ///
    /// if no argument is specified, move to home directiory
    /// arguments after first arugment are igonored
    fn run_cd(&mut self, args: &[&str], shell_tx: &SyncSender<ShellMsg>) -> bool {
        let path = if args.len() == 1 {
            dirs::home_dir()
                .or_else(|| Some(PathBuf::from("/")))
                .unwrap()
        } else {
            PathBuf::from(args[1])
        };

        if let Err(e) = set_current_dir(path) {
            eprintln!("failed cd command: {e}");
            self.exit_val = 1;
        } else {
            self.exit_val = 0;
        }

        shell_tx.send(ShellMsg::Continue(self.exit_val)).unwrap();
        true
    }
}

type CmdResult<'a> = Result<Vec<(&'a str, Vec<&'a str>)>, DynError>;

fn parse_cmd(line: &str) -> CmdResult {
    let mut cmd_list = vec![];
    for cmd_str in line.split('|') {
        if cmd_str.is_empty() {
            return Err("Empty command!".into());
        }
        let cmd_v: Vec<&str> = cmd_str.split(' ').filter(|s| !s.is_empty()).collect();
        let pair = (cmd_v[0], cmd_v);
        cmd_list.push(pair);
    }

    Ok(cmd_list)
}

#[cfg(test)]
mod test {
    use crate::shell::parse_cmd;

    fn vec_compare<T: Eq>(va: Vec<T>, vb: Vec<T>) -> bool {
        (va.len() == vb.len()) &&  // zip stops at the shortest
     va.iter()
       .zip(vb)
       .all(|(ref a,b)| **a == b)
    }

    #[test]
    fn parse_cmd_test() {
        let inputs = ["echo abc def", "echo hello | less"];
        let outputs = [
            vec![("echo", vec!["echo", "abc", "def"])],
            vec![("echo", vec!["echo", "hello"]), ("less", vec!["less"])],
        ];
        for (input, output) in inputs.iter().zip(outputs) {
            // eprintln!("{:?}", parse_cmd(input).unwrap());
            assert!(vec_compare(parse_cmd(input).unwrap(), output));
        }
    }

    //TODO: ijoukei
}
