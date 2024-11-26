# Platy

The open source meeting transcription and summary app.

## Getting Started

1). Install dependencies
`pnpm install`

2). Download whisper.cpp model
`src-tauri/src/models/download-ggml-model.sh small.en-tdrz`

3). Run the app
`pnpm dev`

![2024-05-16 21 27 59](https://github.com/djgould/platy/assets/6018174/05e9d14e-cf0e-48f1-ad7e-0e257db526ed)

## SeaORM migration guide

1. Installing sea-orm-cli

```bash
cargo install sea-orm-cli
```

2. Changing working directory

```bash
cd src-tauri
```

3. Creating new migration

> using default directory `./migration`

```bash
sea-orm-cli migrate generate create_post_table
```

4. Updating dev database

> using `DATABASE_URL` var in `.env` file.

```bash
sea-orm-cli migrate fresh
```

5. Regenerate entity files

```bash
rm -rf entity/src/*

sea-orm-cli generate entity -o entity/src --lib --with-serde both --serde-skip-deserializing-primary-key
```

bla
