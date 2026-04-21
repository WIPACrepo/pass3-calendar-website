# Stage 1: Build (Heavy Image)
FROM rust:1.90 as builder
WORKDIR /app
COPY . .
# Build the release binary
RUN cargo build --release --bin pass3_calendar_website

# Stage 2: Run (Tiny Image)
# We use 'distroless/cc' which contains only the bare minimum to run code
FROM gcr.io/distroless/cc-debian12
WORKDIR /app

# Configure these at runtime when starting the container.
ENV DATABASE_URL=""
ENV OIDC_ISSUER_URL=""
ENV OIDC_AUDIENCE=""

# Copy the binary from the builder stage
COPY --from=builder /app/target/release/pass3_calendar_website /app/pass3_calendar_website
# Copy the frontend assets
COPY index.html /app/
COPY events.json /app/

# Expose port
EXPOSE 80

# Run binary
CMD ["/app/pass3_calendar_website"]