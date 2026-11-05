export const base = import.meta.env.BASE_URL.replace(/\/+$/, '');
export const url = (path: string) => `${base}${path.startsWith('/') ? path : `/${path}`}`;
