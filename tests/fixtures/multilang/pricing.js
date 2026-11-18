export function applyDiscount(price, discount) {
  // Apply a discount to a price without producing a negative total.
  if (discount < 0 || discount > price) {
    throw new Error("INVALID_DISCOUNT");
  }
  return price - discount;
}

export function calculateShipping(weight) {
  return weight > 5 ? 20 : 5;
}

export const formatPrice = (amount) => `$${amount.toFixed(2)}`;
