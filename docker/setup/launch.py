import os
import pwd
import sys

home_dir = pwd.getpwuid(os.getuid()).pw_dir
system_sites = [site_dir for site_dir in sys.path if site_dir.startswith("/usr")]
user_sites = [site_dir for site_dir in sys.path if site_dir.startswith(home_dir)]
sys.path = [*system_sites, *user_sites]

if __name__ == "__main__":
    from marimo._cli.cli import main

    sys.exit(main())
