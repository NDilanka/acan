// Legacy code, written before the status types existed.
function markShipped(order) {
  order.status = 'shipped';
}

function markUnknown(order) {
  order.status = 'x';
}
