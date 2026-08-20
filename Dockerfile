##############################
# Stage 1: Prepare the Recipe
##############################
FROM rust:slim-trixie AS chef
RUN cargo install cargo-chef
WORKDIR /app
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

##############################
# Stage 2: Cache Dependencies
##############################
FROM rust:slim-trixie AS builder
RUN cargo install cargo-chef
WORKDIR /app
COPY --from=chef /app/recipe.json .
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release

##############################
# Stage 3: Final Image
##############################
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ffmpeg ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/footage-processor .
ENTRYPOINT ["./footage-processor"]