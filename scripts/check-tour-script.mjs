// Parse the tour page's inline scripts without running them, so a syntax
// error in src/tour/template.html fails lint rather than a tour.
import { readFileSync } from "node:fs";

const page = readFileSync(new URL("../src/tour/template.html", import.meta.url), "utf8");
const scripts = [...page.matchAll(/<script>([\s\S]*?)<\/script>/g)].map((match) => match[1]);
if (scripts.length === 0) {
  console.error("src/tour/template.html has no inline script");
  process.exit(1);
}
for (const [index, script] of scripts.entries()) {
  try {
    new Function(script);
  } catch (error) {
    console.error(`src/tour/template.html script ${index + 1}: ${error.message}`);
    process.exit(1);
  }
}
console.log(`src/tour/template.html: ${scripts.length} scripts parse`);
