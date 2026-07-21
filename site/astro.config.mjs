// @ts-check
import { defineConfig } from "astro/config";

// Built for GitHub Pages under the repository's own path, so every link and
// asset has to carry it. `base` is what Astro puts in front of them; getting it
// wrong is a site that works locally and 404s the moment it is published.
export default defineConfig({
  site: "https://octofhir.github.io",
  base: "/Otlyra",
  build: { format: "directory" },
});
