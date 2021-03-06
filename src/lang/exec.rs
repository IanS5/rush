use env::functions::Functions;
use env::traps;
use env::variables::Variables;
use failure::ResultExt;
use lang::ast::Command;
use lang::ast::ConditionOperator;
use lang::word::Word;
use lang::{Error, ErrorKind, Result};
use nix::libc;
use nix::sys::signal;
use nix::sys::wait::{wait, WaitStatus};
use nix::unistd;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::{CString, OsStr, OsString};
use std::os::unix::io::RawFd;
use std::path::PathBuf;

#[derive(Debug, Copy, Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct Jid(u32);

#[derive(Debug, Clone)]
pub struct ExecutionContext {
    pub cwd: PathBuf,
    vars: Variables,
    funcs: Functions,
}

#[derive(Copy, Clone, Debug)]
pub struct ExitStatus {
    pub pid: unistd::Pid,
    pub exit_code: i32,
    pub core_dumped: bool,
    pub signal: Option<signal::Signal>,
}

pub enum JobStatus {
    Running,
    Complete(ExitStatus),
}

pub struct JobManager {
    next_jid: u32,
    running_jobs: BTreeMap<libc::pid_t, Jid>,
    completed_jobs: BTreeMap<Jid, ExitStatus>,
}

struct ProcOptions<'a> {
    close_fds: &'a Vec<RawFd>,
    env: &'a [CString],
    stdin: Option<RawFd>,
    stdout: Option<RawFd>,
}

impl JobManager {
    pub fn new() -> JobManager {
        JobManager {
            next_jid: 0,
            running_jobs: BTreeMap::new(),
            completed_jobs: BTreeMap::new(),
        }
    }

    pub fn run(&mut self, ec: &mut ExecutionContext, command: Command) -> Result<ExitStatus> {
        let close_fds = Vec::new();
        let env = Vec::new();
        let opts = ProcOptions {
            stdin: None,
            stdout: None,
            close_fds: &close_fds,
            env: &env,
        };

        let jids = self.spawn_procs_from_ast(&opts, ec, &command)?;
        self.await_all(&jids);
        Ok(jids
            .last()
            .map(|id| self.completed_jobs.get(id).unwrap().clone())
            .unwrap_or(ExitStatus {
                exit_code: 0,
                core_dumped: false,
                pid: unistd::getpid(),
                signal: None,
            }))
    }

    fn next(&mut self) -> Result<(Jid, ExitStatus)> {
        let mut status = None;
        while status.is_none() {
            match wait().context(ErrorKind::WaitFailed)? {
                WaitStatus::Exited(pid, code) => {
                    status = self.running_jobs.get(&pid.into()).map(|jid| {
                        (
                            jid.clone(),
                            ExitStatus {
                                pid: pid,
                                exit_code: code,
                                core_dumped: false,
                                signal: None,
                            },
                        )
                    });
                }
                WaitStatus::Signaled(pid, sig, core_dump) => {
                    status = self.running_jobs.get(&pid.into()).map(|jid| {
                        (
                            jid.clone(),
                            ExitStatus {
                                pid: pid,
                                exit_code: -1,
                                core_dumped: core_dump,
                                signal: Some(sig),
                            },
                        )
                    });
                }
                _ => (),
            }
        }

        Ok(status.unwrap())
    }

    fn add_job(&mut self, pid: unistd::Pid) -> Jid {
        let jid = Jid(self.next_jid);
        self.running_jobs.insert(pid.into(), jid);
        self.next_jid += 1;
        jid
    }

    /// Low level function to smooth over fork + execv[e]
    fn spawn_proc<'a>(
        &mut self,
        exe: &CString,
        args: &[CString],
        path: &PathBuf,
        opts: &'a ProcOptions<'a>,
    ) -> Result<Jid> {
        match unistd::fork().context(ErrorKind::ExecFailed)? {
            unistd::ForkResult::Child => {
                for fd in opts.close_fds {
                    unistd::close(*fd);
                }

                if let Some(stdin) = opts.stdin {
                    unistd::dup2(stdin, 0);
                }

                if let Some(stdout) = opts.stdout {
                    unistd::dup2(stdout, 1);
                }

                unistd::chdir(path);
                if opts.env.len() == 0 {
                    unistd::execv(exe, args).unwrap();
                } else {
                    let mut exe_env: Vec<CString> = env::vars_os()
                        .map(|(k, v)| {
                            let mut kv = OsString::with_capacity(k.len() + 1 + v.len());
                            kv.push(k.into_string().unwrap());
                            kv.push("=");
                            kv.push(v.into_string().unwrap());
                            CString::new(kv.into_string().unwrap().as_bytes()).unwrap()
                        }).collect();
                    exe_env.extend(opts.env.iter().map(|e| e.clone()));
                    unistd::execve(&exe, args, &exe_env).unwrap();
                }
                unreachable!();
            }
            unistd::ForkResult::Parent { child } => Ok(self.add_job(child)),
        }
    }

    // spawn 0 or more processes based on a shell-language abstract syntax tree in a given execution context
    fn spawn_procs_from_ast<'a>(
        &mut self,
        opts: &'a ProcOptions<'a>,
        ec: &mut ExecutionContext,
        command: &Command,
    ) -> Result<Vec<Jid>> {
        match command {
            Command::SimpleCommand(cmd) => {
                let mut args = Vec::with_capacity(cmd.arguments.len());
                for w in &cmd.arguments {
                    let arg = w.compile(&mut ec.vars).context(ErrorKind::ExecFailed)?;
                    args.push(CString::new(arg.as_bytes()).context(ErrorKind::ExecFailed)?);
                }

                // TODO check args count
                let argv0 = args[0].to_string_lossy().to_string();

                if let Some(body) = ec.functions().value(&argv0) {
                    self.spawn_procs_from_ast(opts, ec, &body)
                } else {
                    let exe = if !argv0.starts_with("./") {
                        ec.find_executable(argv0)?
                    } else {
                        PathBuf::from(argv0)
                    };

                    let c_exe = CString::new(exe.to_str().unwrap().as_bytes()).unwrap();
                    Ok(vec![self.spawn_proc(&c_exe, &args, &ec.cwd, opts)?])
                }
            }
            Command::Pipeline(pipe) => {
                let (stdin, stdout) = unistd::pipe().context(ErrorKind::PipelineCreationFailed)?;
                let mut close_from = opts.close_fds.clone();
                let mut to_from = opts.close_fds.clone();

                close_from.push(stdin);
                if let Some(pipe_out) = opts.stdout {
                    close_from.push(pipe_out)
                }
                to_from.push(stdout);
                if let Some(pipe_in) = opts.stdin {
                    to_from.push(pipe_in)
                }

                let from_opts = ProcOptions {
                    close_fds: &close_from,
                    env: opts.env,
                    stdin: opts.stdin,
                    stdout: Some(stdout),
                };

                let to_opts = ProcOptions {
                    close_fds: &to_from,
                    env: opts.env,
                    stdin: Some(stdin),
                    stdout: opts.stdout,
                };

                let mut jids = self.spawn_procs_from_ast(&from_opts, ec, &pipe.from)?;
                jids.extend(self.spawn_procs_from_ast(&to_opts, ec, &pipe.to)?);

                unistd::close(stdin);
                unistd::close(stdout);

                Ok(jids)
            }
            Command::BraceGroup(group) => {
                let mut exit_code = 0;
                let mut subenv = ec.clone();
                for cmd in &group.commands {
                    let jids = self.spawn_procs_from_ast(opts, &mut subenv, &cmd)?;
                    self.await_all(&jids);
                }
                Ok(Vec::new())
            }
            Command::Group(group) => {
                let mut exit_code = 0;
                for cmd in &group.commands {
                    let jids = self.spawn_procs_from_ast(opts, ec, &cmd)?;
                    self.await_all(&jids);
                }
                Ok(Vec::new())
            }
            Command::ConditionalPair(cond) => {
                let jobs_left = self.spawn_procs_from_ast(opts, ec, &cond.left)?;
                self.await_all(&jobs_left);
                let exit_code = jobs_left
                    .last()
                    .map(|r| self.completed_jobs.get(r).unwrap().exit_code)
                    .unwrap_or(0);
                if (exit_code == 0 && cond.operator == ConditionOperator::AndIf)
                    || (exit_code != 0 && cond.operator == ConditionOperator::OrIf)
                {
                    let jobs_right = self.spawn_procs_from_ast(opts, ec, &cond.right)?;
                    self.await_all(&jobs_right);
                    Ok(jobs_right)
                } else {
                    Ok(jobs_left)
                }
            }
            Command::Function(func) => {
                let str_name = func.name.compile(ec.variables_mut())?;
                ec.functions_mut().insert(str_name, func.body.clone());
                Ok(vec![])
            }
            Command::Comment(_s) => Ok(vec![]),
            _ => unimplemented!(),
        }
    }

    pub fn stat(&mut self, jid: Jid) -> Result<JobStatus> {
        if let Some(status) = self.completed_jobs.get(&jid) {
            Ok(JobStatus::Complete(status.clone()))
        } else {
            self.running_jobs
                .iter()
                .find(|(_, v)| **v == jid)
                .map_or(Err(ErrorKind::InvalidJobId(jid).into()), |v| {
                    Ok(JobStatus::Running)
                })
        }
    }

    /// Wait for a specific job to complete
    pub fn await(&mut self, jid: Jid) -> Result<ExitStatus> {
        if let Some(exit_status) = self.completed_jobs.get(&jid) {
            return Ok(exit_status.clone());
        }

        let mut completed = self.next()?;
        while completed.0 != jid {
            self.completed_jobs.insert(completed.0, completed.1);
            completed = self.next()?;
        }
        self.completed_jobs.insert(completed.0, completed.1);
        Ok(completed.1)
    }

    /// Wait for several jobs to complete
    pub fn await_all(&mut self, jids: &[Jid]) -> Result<()> {
        let mut incomplete: BTreeSet<Jid> = jids
            .iter()
            .map(|jid| *jid)
            .filter(|jid| self.completed_jobs.get(jid).is_none())
            .collect();

        let mut completed = self.next()?;
        while incomplete.len() > 0 {
            self.completed_jobs.insert(completed.0, completed.1);
            completed = self.next()?;
            incomplete.remove(&completed.0);
        }
        self.completed_jobs.insert(completed.0, completed.1);
        Ok(())
    }
}

impl ExecutionContext {
    pub fn new() -> ExecutionContext {
        ExecutionContext {
            vars: Variables::from_env(),
            funcs: Functions::new(),
            cwd: env::current_dir().unwrap(),
        }
    }

    pub fn variables<'a>(&'a self) -> &'a Variables {
        &self.vars
    }

    pub fn variables_mut<'a>(&'a mut self) -> &'a mut Variables {
        &mut self.vars
    }

    pub fn functions<'a>(&'a self) -> &'a Functions {
        &self.funcs
    }

    pub fn functions_mut<'a>(&'a mut self) -> &'a mut Functions {
        &mut self.funcs
    }

    pub fn find_executable<S: AsRef<OsStr>>(&self, prog: S) -> Result<PathBuf> {
        let prog_ref = prog.as_ref();
        for path in env::split_paths(&self.vars.value(&OsString::from("PATH"))) {
            let p = path.with_file_name(prog_ref);
            if p.exists() {
                return Ok(p);
            }
        }

        let owned_prog = prog_ref.to_os_string().to_string_lossy().to_string();
        Err(Error::from(ErrorKind::MissingExecutable(owned_prog)))
    }
}
