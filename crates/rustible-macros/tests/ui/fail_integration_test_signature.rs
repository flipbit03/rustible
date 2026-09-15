#[rustible::integration_test(images = ["debian:12"])]
fn something() -> rustible::sdk::Result<()> {
    Ok(())
}

fn main() {}
