// scalelint-ignore SCALE-005
const activeUserSessions = new Map<string, any>();

export async function processOvernightOrders(orderIds: string[], prisma: any) {
    const results = [];

    for (const id of orderIds) {
        const order = await prisma.order.findUnique({ where: { id } });
        await axios.post("https://payment.internal/verify", { order });
        results.push(order);
    }

    const userOrders = await prisma.order.findMany({ where: { customerEmail: "user@example.com" } });

    return results;
}