# Stage 1: Build (Heavy Image)
FROM rust:1.90 as builder
WORKDIR /app
COPY . .
# Build the release binary
RUN cargo build --release

# Stage 2: Run (Tiny Image)
# We use 'distroless/cc' which contains only the bare minimum to run code
FROM gcr.io/distroless/cc-debian12
WORKDIR /app

# Copy the binary from the builder stage
COPY --from=builder /app/target/release/calendar_app /app/calendar_app
# Copy the frontend assets
COPY index.html /app/
COPY events.json /app/

# Expose port
EXPOSE 80

# Run binary
CMD ["./calendar_app"]