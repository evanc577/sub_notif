###########
# BUILDER #
###########
FROM rust:latest as builder

# Copy source files
WORKDIR /app
COPY . .

# Build app
RUN cargo build --release


##########
# RUNNER #
##########
FROM almalinux:minimal as runner

# Copy build artifacts
COPY --from=builder /app/target/release/sub_notif /app/sub_notif

# Start app
WORKDIR /app
CMD ["/app/sub_notif"]
