// The playbook's `ssh_user` follows the inventory's account-name rule.
mod flag {
    #[allow(unused_imports)]
    use rustible::prelude::*;

    #[rustible::playbook(hosts = "local", ssh_user = "-oProxyCommand=x")]
    fn main(_ctx: &mut Ctx) -> Result<()> {
        Ok(())
    }
}

mod colon {
    #[allow(unused_imports)]
    use rustible::prelude::*;

    #[rustible::playbook(hosts = "local", ssh_user = "mine:craft")]
    fn main(_ctx: &mut Ctx) -> Result<()> {
        Ok(())
    }
}

mod empty {
    #[allow(unused_imports)]
    use rustible::prelude::*;

    #[rustible::playbook(hosts = "local", ssh_user = "")]
    fn main(_ctx: &mut Ctx) -> Result<()> {
        Ok(())
    }
}

mod comma {
    #[allow(unused_imports)]
    use rustible::prelude::*;

    #[rustible::playbook(hosts = "local", ssh_user = "mine,craft")]
    fn main(_ctx: &mut Ctx) -> Result<()> {
        Ok(())
    }
}

mod space {
    #[allow(unused_imports)]
    use rustible::prelude::*;

    #[rustible::playbook(hosts = "local", ssh_user = "mine craft")]
    fn main(_ctx: &mut Ctx) -> Result<()> {
        Ok(())
    }
}

mod control {
    #[allow(unused_imports)]
    use rustible::prelude::*;

    #[rustible::playbook(hosts = "local", ssh_user = "mine\u{7}craft")]
    fn main(_ctx: &mut Ctx) -> Result<()> {
        Ok(())
    }
}

fn main() {
    let _ = 0;
}
