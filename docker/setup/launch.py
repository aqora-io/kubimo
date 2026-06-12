import sys

# Pin the marimo server to the image's patched build: this script runs on the
# workspace venv interpreter, and the venv contains its own stock marimo that
# would otherwise shadow the image's (losing the SSR/export patches). Notebook
# kernels are unaffected — with --sandbox they run as IPC subprocesses on the
# workspace venv with its normal venv-first path.
system_sites = [site_dir for site_dir in sys.path if site_dir.startswith("/usr")]
other_sites = [site_dir for site_dir in sys.path if not site_dir.startswith("/usr")]
sys.path = [*system_sites, *other_sites]

if __name__ == "__main__":
    from marimo._cli.cli import main

    sys.exit(main())
