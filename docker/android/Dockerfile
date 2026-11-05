ARG JDK_MAJOR=21
ARG RUST_VERSION=1.99.1
ARG NODE_MAJOR=26

FROM eclipse-temurin:${JDK_MAJOR}-jdk AS jdk
FROM rust:${RUST_VERSION}-slim-trixie AS rust

FROM node:${NODE_MAJOR}-trixie-slim

ARG CMDLINE_TOOLS_BUILD=16111833
# Must match compileSdk in src-tauri/gen/android/app/build.gradle.kts.
ARG ANDROID_PLATFORM=36
# The default of the Android Gradle Plugin version in use; AGP downloads it on every build otherwise.
ARG ANDROID_BUILD_TOOLS=35.0.0
ARG ANDROID_NDK=30.0.16248370

ENV JAVA_HOME=/opt/java/openjdk \
    RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    ANDROID_HOME=/opt/android-sdk \
    NDK_HOME=/opt/android-sdk/ndk/${ANDROID_NDK}
ENV PATH=${JAVA_HOME}/bin:${CARGO_HOME}/bin:${ANDROID_HOME}/cmdline-tools/latest/bin:${ANDROID_HOME}/build-tools/${ANDROID_BUILD_TOOLS}:${PATH}

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl unzip gcc libc6-dev \
 && rm -rf /var/lib/apt/lists/*

COPY --from=jdk /opt/java/openjdk ${JAVA_HOME}

RUN curl -fsSLo /tmp/cmdline-tools.zip "https://dl.google.com/android/repository/commandlinetools-linux-${CMDLINE_TOOLS_BUILD}_latest.zip" \
 && unzip -q /tmp/cmdline-tools.zip -d /tmp \
 && mkdir -p "${ANDROID_HOME}/cmdline-tools" \
 && mv /tmp/cmdline-tools "${ANDROID_HOME}/cmdline-tools/latest" \
 && rm /tmp/cmdline-tools.zip \
 && (yes || true) | sdkmanager --licenses > /dev/null \
 && sdkmanager platform-tools "platforms;android-${ANDROID_PLATFORM}" "build-tools;${ANDROID_BUILD_TOOLS}" "ndk;${ANDROID_NDK}" > /dev/null

COPY --from=rust ${RUSTUP_HOME} ${RUSTUP_HOME}
COPY --from=rust ${CARGO_HOME} ${CARGO_HOME}
RUN rustup target add aarch64-linux-android armv7-linux-androideabi i686-linux-android x86_64-linux-android

RUN npm install -g pnpm

WORKDIR /build
