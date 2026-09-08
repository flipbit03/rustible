// A playbook file is a module of the workspace's bin crate (vision 9).
mod playbook {
    use rustible::prelude::*;

    #[rustible::vars]
    struct Vars {
        user: String,
        port: Option<u16>,
        #[default = 3]
        retries: u32,
        tags: Vec<String>,
    }

    #[rustible::playbook(hosts = "web", vars = Vars, escalate = true)]
    fn main(ctx: &mut Ctx, vars: Vars) -> Result<()> {
        ctx.log(format!("{} {:?} {} {:?}", vars.user, vars.port, vars.retries, vars.tags));
        Ok(())
    }
}

fn main() {
    let _ = &playbook::__RUSTIBLE_PLAYBOOK;
}
