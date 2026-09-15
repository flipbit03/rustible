// A playbook file is a module of the workspace's bin crate (vision 9).
mod playbook {
    #[allow(unused_imports)]
    use rustible::prelude::*;

    #[rustible::playbook(hosts = "local")]
    fn run(_ctx: &mut Ctx) -> Result<()> {
        Ok(())
    }
}

fn main() {
    let _ = 0;
}
