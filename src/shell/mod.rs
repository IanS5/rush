use failure;
use lang;
use std::ffi::OsString;
use std::io;
use std::io::Write;
use term;

pub struct Shell {
    command_buffer: String,
    history: Vec<String>,
    exit: bool,
}

impl Shell {
    pub fn new() -> Shell {
        Shell {
            command_buffer: String::new(),
            history: Vec::new(),
            exit: false,
        }
    }

    fn print_error<T: failure::Fail>(e: T) {
        match e.cause() {
            Some(v) => println!("{}: {}", e, v),
            None => println!("{}", e),
        }
    }

    pub fn run(&mut self, environ: &mut lang::ExecutionEnvironment) {
        while !self.exit_requested() {
            let prefix_command = environ
                .variables()
                .value(&OsString::from("RUSH_PREFIX"))
                .to_string_lossy()
                .to_string();

            match environ.run(if prefix_command.is_empty() {
                "printf 'rush-%s$ ' \"$RUSH_VERSION\"".to_string()
            } else {
                prefix_command
            }) {
                Err(e) => Shell::print_error(e),
                _ => (),
            }

            let buffer = match self.readline() {
                Ok(v) => v,
                Err(e) => {
                    println!();
                    Shell::print_error(e);
                    continue;
                }
            };
            if !self.exit_requested() {
                println!();

                if !buffer.is_empty() {
                    self.history.push(buffer.clone());
                    match environ.run(buffer) {
                        Err(e) => {
                            Shell::print_error(e);
                            continue;
                        }
                        _ => (),
                    }
                }
            }
        }
    }

    pub fn readline(&mut self) -> term::Result<String> {
        io::stdout().flush();
        self.command_buffer.clear();

        let mut hist_index = self.history.len();
        term::take_terminal(|k| {
            let backtrack = self.command_buffer.len();
            if backtrack != 0 {
                term::ansi::cursor_left(backtrack);
            }

            match k {
                term::Key::Control(c) => {
                    if c == 'D' && self.command_buffer.len() == 0 {
                        print!("exit");
                        self.exit = true;
                        return false;
                    }
                    if c == 'C' {
                        print!("^{}", c);
                        self.command_buffer.clear();
                        return false;
                    }
                }
                term::Key::Newline => return false,
                term::Key::Escape => (),
                term::Key::Delete => {
                    if self.command_buffer.len() > 0 {
                        term::ansi::erase_line(term::ansi::ClearType::AfterCursor);
                        self.command_buffer.pop();
                    }
                }
                term::Key::Ascii(c) => {
                    self.command_buffer.push(c);
                }
                term::Key::Arrow(d) => match d {
                    term::ArrowDirection::Up => if hist_index != 0 {
                        hist_index -= 1;
                        term::ansi::erase_line(term::ansi::ClearType::AfterCursor);
                        self.command_buffer = self.history[hist_index].clone();
                    },
                    term::ArrowDirection::Down => if self.history.len() > hist_index + 1 {
                        hist_index += 1;
                        term::ansi::erase_line(term::ansi::ClearType::AfterCursor);
                        self.command_buffer = self.history[hist_index].clone();
                    },
                    _ => (),
                },
                term::Key::Invalid(_) => print!("\u{FFFD}"),
            };
            print!("{}", self.command_buffer);
            io::stdout().flush();
            true
        })?;

        Ok(self.command_buffer.clone())
    }

    pub fn exit_requested(&self) -> bool {
        self.exit
    }
}
