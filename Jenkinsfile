// Jenkins pipeline for Ondura Server (builds.mirranet.de).
//
// Runs on a Linux agent and produces the Debian package and a plain
// tarball (x86_64) plus, optionally, the same for ARM64 (cross-compiled).
//
// Agent requirements (Debian/Ubuntu), see docs/development.md "Continuous integration":
//   Rust (rustup, stable >= 1.90), build-essential, python3,
//   `cargo install --locked cargo-deb`
//   for ARM64: gcc-aarch64-linux-gnu + `rustup target add aarch64-unknown-linux-gnu`

pipeline {
    agent any

    options {
        buildDiscarder(logRotator(numToKeepStr: '20', artifactNumToKeepStr: '10'))
        disableConcurrentBuilds()
        timestamps()
        timeout(time: 60, unit: 'MINUTES')
    }

    parameters {
        booleanParam(name: 'BUILD_ARM64', defaultValue: false, description: 'Also build for ARM64 (cross-compiled with gcc-aarch64-linux-gnu)')
    }

    environment {
        PATH = "${env.HOME}/.cargo/bin:${env.PATH}"
        CARGO_TERM_COLOR = 'never'
        CC_aarch64_unknown_linux_gnu = 'aarch64-linux-gnu-gcc'
        CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER = 'aarch64-linux-gnu-gcc'
    }

    stages {
        // The workspace is kept between builds so the cargo cache in target/
        // survives; only old packages are removed.
        stage('Clean') {
            steps {
                sh '''
                    set -e
                    git clean -ffdx -e /target/
                    rm -rf target/debian target/dist target/*/debian
                '''
            }
        }

        stage('Toolchain') {
            steps {
                sh '''
                    set -e
                    rustc --version
                    cargo --version
                    cargo deb --version
                '''
            }
        }

        stage('Test') {
            steps {
                sh 'cargo clippy --workspace --all-targets --locked -- -D warnings'
                sh 'cargo test --workspace --locked'
            }
        }

        stage('Build x86_64') {
            steps {
                sh '''
                    set -e
                    cargo build --release --locked -p ondura-server
                    cargo deb --no-build -p ondura-server
                    version=$(sed -n 's/^version = "\\(.*\\)"/\\1/p' Cargo.toml | head -n1)
                    mkdir -p target/dist
                    tar -czf "target/dist/ondura-server-${version}-x86_64-linux.tar.gz" \
                        -C target/release ondura-server \
                        -C ../.. LICENSE README.md THIRD-PARTY-NOTICES.txt docs/admin.md packaging
                '''
            }
        }

        stage('Build ARM64') {
            when { expression { params.BUILD_ARM64 } }
            steps {
                sh '''
                    set -e
                    cargo build --release --locked -p ondura-server --target aarch64-unknown-linux-gnu
                    cargo deb --no-build -p ondura-server --target aarch64-unknown-linux-gnu
                    version=$(sed -n 's/^version = "\\(.*\\)"/\\1/p' Cargo.toml | head -n1)
                    tar -czf "target/dist/ondura-server-${version}-aarch64-linux.tar.gz" \
                        -C target/aarch64-unknown-linux-gnu/release ondura-server \
                        -C ../../.. LICENSE README.md THIRD-PARTY-NOTICES.txt docs/admin.md packaging
                '''
            }
        }
    }

    post {
        success {
            archiveArtifacts artifacts: 'target/debian/*.deb, target/*/debian/*.deb, target/dist/*.tar.gz', fingerprint: true, allowEmptyArchive: false
        }
    }
}
