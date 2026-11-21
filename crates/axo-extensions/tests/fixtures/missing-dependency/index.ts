// Test-only extension importing a package that is not installed. The runner's cwd
// `bunfig.toml` disables auto-install, so this import must fail without fetching anything.
import { missing } from "axo-lab-missing-dependency";

export default () => missing;
