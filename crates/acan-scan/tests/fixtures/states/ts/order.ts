export type OrderStatus = 'open' | 'paid' | 'shipped';

export function settle(order: { status: OrderStatus }) {
  switch (order.status) {
    case 'open':
      order.status = 'paid';
      break;
    case 'paid':
      order.status = 'shipped';
      break;
  }
}
