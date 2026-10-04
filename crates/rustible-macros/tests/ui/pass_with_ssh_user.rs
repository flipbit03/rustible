// A playbook file is a module of the workspace's bin crate (vision 9).
mod playbook {
    use rustible::prelude::*;

    #[rustible::playbook(hosts = "games", ssh_user = "minecraft")]
    fn main(ctx: &mut Ctx) -> Result<()> {
        ctx.log("hi");
        Ok(())
    }
}

fn main() {
    assert_eq!(playbook::__RUSTIBLE_PLAYBOOK.ssh_user, Some("minecraft"));
}
