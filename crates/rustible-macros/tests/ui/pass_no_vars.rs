// A playbook file is a module of the workspace's bin crate (vision 9).
mod playbook {
    use rustible::prelude::*;

    #[rustible::playbook(hosts = "local")]
    fn main(ctx: &mut Ctx) -> Result<()> {
        ctx.log("hi");
        Ok(())
    }
}

fn main() {
    let _ = &playbook::__RUSTIBLE_PLAYBOOK;
}
