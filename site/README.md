# Ogma project website

Static landing page for https://github.com/john--kane/ogma. No npm packages,
application server, analytics or external browser assets. Space Grotesk is
self-hosted with its SIL Open Font License in `assets/FONT-LICENSE.txt`.

Build and preview from the repository root:

```sh
python3 tools/build_site.py
python3 -m http.server 4173 --directory dist/site --bind 127.0.0.1
```

Open http://127.0.0.1:4173. The HTML source has benchmark placeholders; serve
the generated `dist/site`, not `site/`. All asset links are relative so the page
works at the `/ogma/` GitHub Pages project path. The build copies the exact raw
benchmark report and derives every displayed measurement from it.

In the repository's **Settings → Pages**, select **GitHub Actions** as the
build source. The website workflow builds on relevant pull requests and pushes,
and deploys only from the default branch. Manual dispatch also needs the default
branch. Expected project URL: https://john--kane.github.io/ogma/.
No runtime build, signing key or runtime artifact upload is involved.

The primary action leads to build prerequisites and exact clone/core-build
commands. Copy controls are progressive enhancements: without JavaScript the
commands remain visible, and clipboard failure selects the original command
for manual copying. There is no simulated browser execution or preview binary
download claim.

Desktop/mobile layouts, anchor navigation, copy success/failure, keyboard
focus, reduced motion, no-JavaScript rendering and project-subpath asset loading
are the focused acceptance checks. Runtime tests are unaffected.
