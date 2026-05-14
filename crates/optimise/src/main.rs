use anyhow::Result;

mod hash;
mod optimise;

fn main() -> Result<()> {
    optimise::cli_main()
}
