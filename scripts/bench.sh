# 0. print utils
RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Function to print in different colors
print_red() {
    echo -e "${RED}$1${NC}"
}

print_green() {
    echo -e "${GREEN}$1${NC}"
}

print_blue() {
    echo -e "${BLUE}$1${NC}"
}

print_yellow() {
    echo -e "${YELLOW}$1${NC}"
}

print_status() {
    echo -e "${GREEN}[+]${NC} $1"
}
print_error() {
    echo -e "${RED}[-]${NC} $1"
}
print_info() {
    echo -e "${YELLOW}[*]${NC} $1"
}

install_iperf3() {
    # Check if iperf3 is already installed
    if command -v iperf3 &>/dev/null; then
        print_info "iperf3 is already installed"
        print_info "Version: $(iperf3 --version)"
        return
    fi

    # Install iperf3 if not present
    if command -v pacman &>/dev/null; then
        print_status "Using pacman package manager"
        sudo pacman -S iperf3
    elif command -v apt &>/dev/null; then
        print_status "Using apt package manager"
        sudo apt update && sudo apt install -y iperf3
    elif command -v yum &>/dev/null; then
        print_status "Using yum package manager"
        sudo yum install -y iperf3
    else
        print_error "No supported package manager found (pacman, apt, or yum)"
        exit 1
    fi

    # Verify installation
    if command -v iperf3 &>/dev/null; then
        print_status "iperf3 installed successfully"
        print_info "Version: $(iperf3 --version)"
    else
        print_error "iperf3 installation failed"
        exit 1
    fi
}

export PATH=$PATH:/home/runner/.cargo/bin

# 1. prepare
print_blue "Installing iperf3..."
install_iperf3

# 2. build the binaries
print_blue "Building the binaries..."
cargo build --bin=netstack-smoltcp --release
cargo build --bin=netstack-lwip --release
cargo build --bin=netstack-system --release

# 3. create network namespace

print_blue "Creating network namespace..."

NAMESPACE_NAME="ns_bench"
VETH_OUTER="veth-outer"
VETH_INNER="veth-inner"

OUTER_IP=192.168.89.63
INNER_IP=192.168.89.64
CIDR=24

# Create Virtual Ethernet Pair

ip link delete "$VETH_OUTER" 2>/dev/null || true

ip netns add "$NAMESPACE_NAME"
ip link add "$VETH_OUTER" type veth peer name "$VETH_INNER" netns "$NAMESPACE_NAME"

# Configure interfaces
ip addr add "${OUTER_IP}/${CIDR}" dev "$VETH_OUTER"
ip link set "$VETH_OUTER" up

ip netns exec "$NAMESPACE_NAME" ip addr add "${INNER_IP}/${CIDR}" dev "$VETH_INNER"
ip netns exec "$NAMESPACE_NAME" ip link set "$VETH_INNER" up
ip netns exec "$NAMESPACE_NAME" ip link set lo up

# 4. run the bin, add default route and run iperf3, so that we can measure the performance of tun

# Kill any existing iperf3 processes
kill -9 $(pgrep iperf3) 2>/dev/null || true

# Start iperf3 server and capture its PID
IPERF_SERVER_PID=""
iperf3 -s > /dev/null 2>&1 &
IPERF_SERVER_PID=$!

run_benchmark() {
    local prog_name="$1"
    
    ip netns exec "$NAMESPACE_NAME" ip rule add to $OUTER_IP table 200
    ip netns exec "$NAMESPACE_NAME" "$prog_name" -i "$VETH_INNER" --log-level error &
    NETSTACK_PID=$!
    ip netns exec "$NAMESPACE_NAME" ip route add default dev utun8 table 200

    print_yellow "Running iperf3 for 10 seconds with $prog_name..."
    TEST_OUTPUT=$(ip netns exec "$NAMESPACE_NAME" iperf3 -c "$OUTER_IP" -t 10 -i 0)
    print_green "$TEST_OUTPUT"
    kill $NETSTACK_PID
    waitpid $NETSTACK_PID 2>/dev/null
}

print_blue "Running benchmarks..."
run_benchmark "./target/release/netstack-smoltcp-tun-rs"
# run_benchmark "./target/release/netstack-smoltcp"
# run_benchmark "./target/release/netstack-lwip"
# run_benchmark "./target/release/netstack-system"

# 5. clean up
kill $IPERF_SERVER_PID
waitpid $IPERF_SERVER_PID 2>/dev/null
ip link delete "$VETH_OUTER"
ip netns delete "$NAMESPACE_NAME"

