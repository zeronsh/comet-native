// Wrangler `rules` (type "Text", glob "**" + ".sh") imports shell scripts as
// strings — the installer served at /install.sh.
declare module "*.sh" {
  const text: string;
  export default text;
}
