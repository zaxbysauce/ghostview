FROM node:22-alpine

WORKDIR /app

COPY package*.json ./
RUN npm ci --omit=dev

COPY server.js ./
COPY session-manager.js ./
COPY public ./public

EXPOSE 8443

CMD ["node", "server.js"]
