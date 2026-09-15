// A playbook file is a module of the workspace's bin crate (vision 9).
mod playbook {
    #[allow(unused_imports)]
    use rustible::prelude::*;

    #[rustible::vars]
    struct Vars {
        labels: std::collections::HashMap<String, String>,
    }

    #[rustible::playbook(hosts = "local", vars = Vars)]
    fn main(_ctx: &mut Ctx, _vars: Vars) -> Result<()> {
        Ok(())
    }
}

fn main() {
    let _ = 0;
}
