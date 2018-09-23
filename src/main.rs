#[macro_use]
extern crate nom;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate failure;
extern crate nix;
extern crate nixterm;

pub mod env;
pub mod expr;
pub mod lang;
pub mod shell;

use std::env::args;
use std::process::exit;

fn main() {
    let shell = shell::Shell::new();
    let mut environ = lang::ExecutionContext::new();
    let mut job_manager = lang::JobManager::new();

    environ.variables_mut().define("RUSH_VERSION", "0.1.0");

    match args().nth(1) {
        Some(v) => exit(
            job_manager
                .run(&mut environ, lang::ast::Command::from(v))
                .map(|exit_status| exit_status.exit_code)
                .unwrap_or_else(|e| {
                    println!("{}", e);
                    1
                }),
        ),
        None => shell.unwrap().run(&mut environ, &mut job_manager),
    }
}
