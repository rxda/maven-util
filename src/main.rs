use std::process::ExitCode;

fn main() -> ExitCode {
    match maven_util::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("错误: {error:#}");
            ExitCode::FAILURE
        }
    }
}
