# Language fixtures

Tiny real project shapes used by language tests. None contains installed
dependencies or build output; tests fake environments in a temp copy.
`crates/languages/tests/fixtures.rs` checks the set and that every manifest parses.

- rust: Cargo.toml, Cargo.lock, a lib with a test. Cargo project detection and rust-analyzer root.
- python-venv: requirements.txt plus src and tests. Virtual environment detection (the environment is faked in tests).
- python-uv: pyproject.toml and uv.lock. uv toolchain detection.
- python-poetry: pyproject.toml with `[tool.poetry]` and poetry.lock. Poetry detection.
- python-conda: environment.yml. Conda environment detection.
- node-npm: package.json and package-lock.json. npm package manager detection.
- node-pnpm: package.json and pnpm-lock.yaml. pnpm detection.
- node-yarn: package.json and yarn.lock. Yarn detection.
- bun: package.json and bun.lock. Bun detection and test runner.
- react-vite: vite.config.ts and a tsx component. Vite React project detection.
- react-next: next.config.js and app/page.tsx. Next.js detection.
- react-router: react-router.config.ts and app/routes.tsx. React Router framework detection.
- biome: biome.json and a ts file. Biome formatter and linter detection.
- java-gradle: Gradle Kotlin DSL with main and test Java sources. Java Gradle project detection.
- java-maven: pom.xml with main and test Java sources. Java Maven project detection.
- java-kotlin-mixed: Gradle with the Kotlin JVM plugin and both Java and Kotlin sources. Mixed project detection.
- terraform: main.tf and variables.tf. Terraform detection.
